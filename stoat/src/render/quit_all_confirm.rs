use crate::{quit_all_confirm::QuitAllConfirm, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders},
};

/// Buffers the modal names at once. A longer list is truncated rather than
/// growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 10;

/// Rows the chrome takes inside the border above the list, which are the
/// prompt naming how many buffers are unsaved and the blank line under it.
const PROMPT_ROWS: u16 = 2;

/// Lay the quit-confirm modal out within `area`, returning its outer box and
/// the inner rect holding the prompt and buffer rows, or [`None`] when `area`
/// is too small to host it.
///
/// Sized even for an empty list, unlike the pickers. The modal's question
/// stands on its own, and the caller only opens it when something is unsaved.
fn quit_all_confirm_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let content_height = 2 + PROMPT_ROWS + entry_rows;
    let modal = crate::render::chrome::modal_box(area, (0, content_height), (70, 5), (50, 5), 0)?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

pub(crate) fn render_quit_all_confirm(
    modal: &QuitAllConfirm,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let entries = modal.entries();
    let Some((modal_area, inner)) = quit_all_confirm_layout(area, entries.len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    crate::render::clear_themed(modal_area, buf, theme);
    crate::render::chrome::modal_frame(
        buf,
        modal_area,
        Some(" unsaved buffers "),
        modal_style,
        theme,
        scene,
    );

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);

    let prompt_row = inner.y;
    let prompt = if entries.len() == 1 {
        "1 buffer has unsaved changes:".to_string()
    } else {
        format!("{} buffers have unsaved changes:", entries.len())
    };
    write_str(buf, inner.x, prompt_row, &prompt, prompt_style);

    let entries_top = inner.y + PROMPT_ROWS;
    let rows = inner.height.saturating_sub(PROMPT_ROWS) as usize;
    let max_display = (inner.width as usize).saturating_sub(3);
    for (i, entry) in entries.iter().take(rows).enumerate() {
        let row = entries_top + i as u16;
        let trimmed: String = entry.display.chars().take(max_display).collect();
        write_str(buf, inner.x, row, " * ", row_style);
        write_str(buf, inner.x + 3, row, &trimmed, row_style);
    }
}

#[cfg(test)]
mod tests {
    use super::{quit_all_confirm_layout, MAX_ENTRY_ROWS, PROMPT_ROWS};
    use ratatui::layout::Rect;

    #[test]
    fn layout_holds_the_prompt_above_one_row_per_buffer() {
        let (modal, inner) =
            quit_all_confirm_layout(Rect::new(0, 0, 100, 40), 3).expect("the area hosts the modal");
        assert_eq!(modal.width, 70, "the box holds at its recommended width");
        assert_eq!(
            inner.height,
            PROMPT_ROWS + 3,
            "the prompt sits above one row per unsaved buffer"
        );
    }

    #[test]
    fn layout_caps_the_buffers_it_names() {
        let (_, inner) = quit_all_confirm_layout(Rect::new(0, 0, 100, 40), 30)
            .expect("the area hosts the modal");
        assert_eq!(inner.height, PROMPT_ROWS + MAX_ENTRY_ROWS);
    }

    /// Unlike the pickers, an empty list still lays out. The modal asks a
    /// question that stands without a list under it.
    #[test]
    fn layout_sizes_an_empty_list() {
        let (modal, inner) =
            quit_all_confirm_layout(Rect::new(0, 0, 100, 40), 0).expect("the area hosts the modal");
        assert_eq!(
            modal.height, 5,
            "content this short falls under the recommended height, which wins"
        );
        assert!(
            inner.height > PROMPT_ROWS,
            "so the prompt never sits flush against the bottom border"
        );
    }

    #[test]
    fn layout_none_when_the_area_cannot_host_the_minimum() {
        assert_eq!(quit_all_confirm_layout(Rect::new(0, 0, 40, 24), 3), None);
        assert_eq!(quit_all_confirm_layout(Rect::new(0, 0, 100, 6), 3), None);
    }
}
