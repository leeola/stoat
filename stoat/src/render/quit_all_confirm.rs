use crate::{quit_all_confirm::QuitAllConfirm, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders},
};

/// Buffers the modal names at once. A longer list is truncated rather than
/// growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 10;

/// Rows the chrome takes inside the border above the list, which are the
/// prompt naming how many buffers are unsaved and the blank line under it.
const PROMPT_ROWS: u16 = 2;

/// The box's title, which the frame writes into its top border and the plate
/// writes, trimmed and bold, before the prompt.
const TITLE: &str = " unsaved buffers ";

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

/// Paint the quit confirm, which names the unsaved buffers, in a box centered
/// over `area`.
///
/// Under the plate look of [`plate_colors`](crate::render::chrome::plate_colors),
/// the box is a plate whose edges stand at
/// [`CONFIRM_EDGES`](crate::render::chrome::CONFIRM_EDGES) around its rows, and
/// the title leads the prompt row in bold. Otherwise the title sits in the
/// frame's top border.
pub(crate) fn render_quit_all_confirm(
    modal: &QuitAllConfirm,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let entries = modal.entries();
    let Some((modal_area, inner)) = quit_all_confirm_layout(area, entries.len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let plate = crate::render::chrome::plate_colors(theme, scene);
    match plate {
        Some(plate) => crate::render::chrome::modal_plate(
            buf,
            inner,
            crate::render::chrome::CONFIRM_EDGES,
            plate,
            theme,
            scene,
        ),
        None => {
            crate::render::clear_modal(modal_area, buf, theme, modal_style);
            crate::render::chrome::modal_frame(
                buf,
                modal_area,
                Some(TITLE),
                modal_style,
                theme,
                scene,
            );
        },
    }

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);

    let prompt_row = inner.y;
    let prompt = if entries.len() == 1 {
        "1 buffer has unsaved changes:".to_string()
    } else {
        format!("{} buffers have unsaved changes:", entries.len())
    };
    let mut prompt_x = inner.x;
    if plate.is_some() {
        let title = TITLE.trim();
        write_str(
            buf,
            prompt_x,
            prompt_row,
            title,
            row_style.add_modifier(Modifier::BOLD),
        );
        prompt_x += title.chars().count() as u16 + 1;
    }
    write_str(buf, prompt_x, prompt_row, &prompt, prompt_style);

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
    use super::{quit_all_confirm_layout, render_quit_all_confirm, MAX_ENTRY_ROWS, PROMPT_ROWS};
    use crate::{
        buffer::BufferId, buffer_registry::DirtyBuffer, quit_all_confirm::QuitAllConfirm,
        render::chrome::CONFIRM_EDGES, theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    use std::path::{Path, PathBuf};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command};

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

    /// Under the plate look the box is the plate's two panels over its rows,
    /// with their edges at the confirm's. A plain panel goes first, and a
    /// backdrop in the ground follows. The title leads the prompt row in bold,
    /// one space before the prompt.
    #[test]
    fn the_plate_box_leads_its_prompt_with_the_title() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.surface.ground.bg = "#0c0e11";
                ui.background.bg = "#17191e";
                ui.text.fg = "#dfe2e8";
                ui.prompt.fg = "#7fb0ff";
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let dirty = |id, path: &str| DirtyBuffer {
            id: BufferId::new(id),
            path: Some(PathBuf::from(path)),
        };
        let modal =
            QuitAllConfirm::new(&[dirty(1, "/r/a.rs"), dirty(2, "/r/b.rs")], Path::new("/r"));
        let area = Rect::new(0, 0, 100, 40);
        let (_, inner) = quit_all_confirm_layout(area, 2).expect("the area hosts the modal");
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        render_quit_all_confirm(&modal, &theme, area, &mut buf, &mut scene);

        let panels: Vec<_> = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((
                    Rect::new(panel.left, panel.top, panel.width, panel.height),
                    panel.fill,
                    panel.edges,
                    panel.backdrop,
                )),
                _ => None,
            })
            .collect();
        let prompt: String = (inner.x..inner.x + 47)
            .map(|x| buf[(x, inner.y)].symbol())
            .collect();
        let style = |x: u16| (buf[(x, inner.y)].fg, buf[(x, inner.y)].modifier);
        let ground = [0x0c, 0x0e, 0x11];
        assert_eq!(
            (panels, prompt, [inner.x, inner.x + 16].map(style)),
            (
                vec![
                    (inner, None, CONFIRM_EDGES, false),
                    (inner, Some(ground), CONFIRM_EDGES, true),
                ],
                "unsaved buffers 2 buffers have unsaved changes:".to_owned(),
                [
                    (Color::Rgb(0xdf, 0xe2, 0xe8), Modifier::BOLD),
                    (Color::Rgb(0x7f, 0xb0, 0xff), Modifier::empty()),
                ],
            ),
        );
    }
}
