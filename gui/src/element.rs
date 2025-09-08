//! Custom editor element for text rendering

use crate::{theme::EditorTheme, vim::VimMode};
use gpui::{div, px, IntoElement, ParentElement, SharedString, Styled};
use stoat::EditorState;

/// Custom element for text editor rendering
pub struct EditorElement {
    state: EditorState,
    vim_mode: VimMode,
    theme: EditorTheme,
    font_size: f32,
    line_height: f32,
    cursor_visible: bool,
    scroll_position: (f32, f32),
}

impl EditorElement {
    pub fn new(
        state: EditorState,
        vim_mode: VimMode,
        theme: EditorTheme,
        font_size: f32,
        line_height: f32,
        cursor_visible: bool,
        scroll_position: (f32, f32),
    ) -> Self {
        Self {
            state,
            vim_mode,
            theme,
            font_size,
            line_height,
            cursor_visible,
            scroll_position,
        }
    }
}

impl IntoElement for EditorElement {
    type Element = gpui::Div;

    fn into_element(self) -> Self::Element {
        let text = self.state.text();
        let lines: Vec<&str> = text.lines().collect();
        let cursor_pos = self.state.cursor_position();
        let cursor_line = cursor_pos.line as usize;
        let cursor_col = cursor_pos.column as usize;

        let start_line = self.scroll_position.0 as usize;
        let line_height_px = px(self.font_size * self.line_height);

        // Create the main editor container
        let mut editor_div = div()
            .bg(self.theme.background)
            .text_color(self.theme.foreground)
            .size_full()
            .flex()
            .flex_col()
            .font_family("JetBrains Mono")
            .text_size(px(self.font_size));

        // Add visible lines
        let visible_lines = 40; // FIXME: Calculate from viewport
        for i in 0..visible_lines {
            let line_idx = start_line + i;
            let mut line_div = div().flex().flex_row().h(line_height_px).w_full();

            // Line number
            line_div = line_div.child(
                div()
                    .w(px(60.0))
                    .pr(px(10.0))
                    .text_color(self.theme.line_number)
                    .text_right()
                    .child(if line_idx < lines.len() {
                        SharedString::from(format!("{:4}", line_idx + 1))
                    } else {
                        SharedString::from("    ")
                    }),
            );

            // Line content
            if line_idx < lines.len() {
                let line_text = lines[line_idx];
                let mut content_div = div().flex_1();

                // Handle cursor rendering
                if line_idx == cursor_line && self.cursor_visible {
                    // Split line at cursor position
                    let (before_cursor, after_cursor) = if cursor_col <= line_text.len() {
                        line_text.split_at(cursor_col)
                    } else {
                        (line_text, "")
                    };

                    content_div = content_div.flex().flex_row();

                    // Text before cursor
                    if !before_cursor.is_empty() {
                        content_div =
                            content_div.child(SharedString::from(before_cursor.to_string()));
                    }

                    // Cursor
                    let cursor_color = match self.vim_mode {
                        VimMode::Insert => self.theme.cursor_insert,
                        VimMode::Normal => self.theme.cursor_normal,
                        VimMode::Visual => self.theme.cursor_visual,
                        VimMode::Command => self.theme.cursor_command,
                    };

                    let cursor_char = if after_cursor.is_empty() {
                        " "
                    } else {
                        &after_cursor[..1.min(after_cursor.len())]
                    };

                    let cursor_width = if self.vim_mode == VimMode::Insert {
                        px(2.0)
                    } else {
                        px(self.font_size * 0.6)
                    };

                    content_div = content_div.child(
                        div()
                            .bg(cursor_color)
                            .w(cursor_width)
                            .child(SharedString::from(cursor_char.to_string())),
                    );

                    // Text after cursor
                    if after_cursor.len() > 1 {
                        content_div =
                            content_div.child(SharedString::from(after_cursor[1..].to_string()));
                    }
                } else if !line_text.is_empty() {
                    content_div = content_div.child(SharedString::from(line_text.to_string()));
                }

                line_div = line_div.child(content_div);
            } else {
                // Empty line with tilde
                line_div = line_div.child(
                    div()
                        .flex_1()
                        .text_color(self.theme.line_number)
                        .child(SharedString::from("~")),
                );
            }

            editor_div = editor_div.child(line_div);
        }

        // Status bar
        let mode_text = match self.vim_mode {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::Command => "COMMAND",
        };

        let position_text = format!("{}:{}", cursor_pos.line + 1, cursor_pos.column + 1);

        editor_div = editor_div.child(
            div()
                .bg(self.theme.status_bar_bg)
                .text_color(self.theme.status_bar_fg)
                .h(line_height_px)
                .w_full()
                .flex()
                .flex_row()
                .justify_between()
                .px(px(10.0))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(SharedString::from(format!(" {} ", mode_text))),
                )
                .child(SharedString::from(position_text)),
        );

        editor_div
    }
}
