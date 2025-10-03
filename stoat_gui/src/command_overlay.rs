use crate::keybinding_hint::KeybindingHint;
use gpui::{div, App, Hsla, IntoElement, ParentElement, RenderOnce, Styled, Window};
use stoat::EditorMode;

/// Overlay displaying available commands for the current editor mode.
///
/// Positioned at the bottom-right of the editor window, this component shows
/// the most relevant keybindings for the active mode (Normal, Insert, Visual).
/// It helps users discover and learn keyboard shortcuts.
#[derive(IntoElement)]
pub struct CommandOverlay {
    /// Current editor mode determining which commands to display
    mode: EditorMode,
}

impl CommandOverlay {
    /// Create a new command overlay for the given mode.
    pub fn new(mode: EditorMode) -> Self {
        Self { mode }
    }

    /// Get the keybinding hints to display for the current mode.
    fn get_hints(&self, bg_color: Hsla) -> Vec<KeybindingHint> {
        match self.mode {
            EditorMode::Normal => vec![
                KeybindingHint::new("h/j/k/l", "move", bg_color),
                KeybindingHint::new("w/b", "word", bg_color),
                KeybindingHint::new("0/$", "line start/end", bg_color),
                KeybindingHint::new("gg/G", "file start/end", bg_color),
                KeybindingHint::new("i", "insert mode", bg_color),
                KeybindingHint::new("v", "visual mode", bg_color),
                KeybindingHint::new("x", "delete char", bg_color),
                KeybindingHint::new("dd", "delete line", bg_color),
                KeybindingHint::new("D", "delete to end", bg_color),
                KeybindingHint::new("u", "undo", bg_color),
            ],
            EditorMode::Insert => vec![
                KeybindingHint::new("Esc", "normal mode", bg_color),
                KeybindingHint::new("Arrows", "move", bg_color),
                KeybindingHint::new("Enter", "new line", bg_color),
                KeybindingHint::new("Backspace", "delete left", bg_color),
                KeybindingHint::new("Delete", "delete right", bg_color),
                KeybindingHint::new("Cmd-S", "save", bg_color),
                KeybindingHint::new("Cmd-Z", "undo", bg_color),
                KeybindingHint::new("Cmd-V", "paste", bg_color),
            ],
            EditorMode::Visual => vec![
                KeybindingHint::new("Esc", "normal mode", bg_color),
                KeybindingHint::new("h/j/k/l", "extend selection", bg_color),
                KeybindingHint::new("w/b", "select word", bg_color),
                KeybindingHint::new("0/$", "select to start/end", bg_color),
                KeybindingHint::new("d", "delete selection", bg_color),
                KeybindingHint::new("y", "copy selection", bg_color),
            ],
        }
    }
}

impl RenderOnce for CommandOverlay {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let bg_color: Hsla = gpui::rgb(0x1E1E1E).into();
        let border_color: Hsla = gpui::rgb(0x404040).into();
        let text_color: Hsla = gpui::rgb(0xE0E0E0).into();

        let hints = self.get_hints(bg_color);

        div()
            .absolute()
            .bottom_4()
            .right_4()
            .p_3()
            .rounded_md()
            .bg(bg_color.opacity(0.95))
            .border_1()
            .border_color(border_color)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(text_color)
                            .mb_1()
                            .child(format!("{}  MODE", self.mode.as_display_str())),
                    )
                    .child(div().flex().flex_col().gap_1().children(hints)),
            )
    }
}
