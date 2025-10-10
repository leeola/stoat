use gpui::{div, App, Hsla, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window};

/// A single keybinding hint showing a key combination and its description.
///
/// Displays keyboard shortcuts in a styled format with the key in a bordered box
/// followed by a description label. Used within [`crate::command_overlay::CommandOverlay`]
/// to show available commands for the current editor mode.
///
/// # Layout
///
/// The component renders as: `[key]` description
/// - Key in bordered box with padding
/// - Description in muted text color
/// - Horizontal flex layout with gap between elements
#[derive(IntoElement)]
pub struct KeybindingHint {
    /// The keyboard shortcut (e.g., "h", "Esc", "Ctrl-w")
    key: SharedString,
    /// Description of what the key does (e.g., "move left", "normal mode")
    description: SharedString,
    /// Background color for the key box
    bg_color: Hsla,
}

impl KeybindingHint {
    /// Create a new keybinding hint.
    ///
    /// # Arguments
    /// * `key` - The keyboard shortcut to display
    /// * `description` - What the keybinding does
    /// * `bg_color` - Background color for the key box
    pub fn new(
        key: impl Into<SharedString>,
        description: impl Into<SharedString>,
        bg_color: Hsla,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            bg_color,
        }
    }
}

impl RenderOnce for KeybindingHint {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let text_color: Hsla = gpui::rgb(0xE0E0E0).into();
        let text_muted: Hsla = gpui::rgb(0xA0A0A0).into();
        let border_color: Hsla = gpui::rgb(0x404040).into();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(border_color)
                    .bg(self.bg_color)
                    .text_color(text_color)
                    .text_xs()
                    .font_family(".SystemUIFont")
                    .child(self.key),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(text_muted)
                    .font_family(".SystemUIFont")
                    .child(self.description),
            )
    }
}
