use crate::keymap::hint::KeybindingHint;
use gpui::{div, App, Hsla, IntoElement, ParentElement, RenderOnce, Styled, Window};

/// Overlay displaying available commands for the current editor mode.
///
/// Positioned at the bottom-right of the editor window, this component shows
/// the most relevant keybindings for the active mode (Normal, Insert, Visual, etc.).
/// The bindings are dynamically queried from the keymap and displayed with their
/// associated help text.
///
/// # Usage
///
/// This component is always rendered in [`crate::editor::view::EditorView`] and
/// updates automatically when the mode changes. Bindings are queried via
/// [`crate::keymap::query::bindings_for_mode`].
///
/// # Layout
///
/// - Positioned absolutely at bottom-right (`.bottom_4().right_4()`)
/// - Semi-transparent dark background (`.bg(bg_color.opacity(0.95))`)
/// - Shows mode name followed by list of keybindings
/// - Each binding rendered as [`KeybindingHint`]
#[derive(IntoElement)]
pub struct CommandOverlay {
    /// Mode display name (e.g., "NORMAL", "INSERT")
    mode_display: String,
    /// Pre-queried bindings for this mode (keystroke, description)
    bindings: Vec<(String, String)>,
}

impl CommandOverlay {
    /// Create a new command overlay with bindings for the given mode.
    ///
    /// # Arguments
    /// * `mode_display` - The mode display name (e.g., "NORMAL")
    /// * `bindings` - Pre-queried keybindings for this mode from
    ///   [`crate::keymap::query::bindings_for_mode`]
    pub fn new(mode_display: String, bindings: Vec<(String, String)>) -> Self {
        Self {
            mode_display,
            bindings,
        }
    }

    /// Convert bindings to keybinding hints for display.
    fn get_hints(&self, bg_color: Hsla) -> Vec<KeybindingHint> {
        self.bindings
            .iter()
            .map(|(key, desc)| KeybindingHint::new(key.clone(), desc.clone(), bg_color))
            .collect()
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
                            .child(format!("{}  MODE", self.mode_display)),
                    )
                    .child(div().flex().flex_col().gap_1().children(hints))
                    .child(
                        div()
                            .mt_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(border_color.opacity(0.5))
                            .text_xs()
                            .text_color(text_color.opacity(0.7))
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_color(gpui::rgb(0x569CD6))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .child("?"),
                                    )
                                    .child("for full help"),
                            ),
                    ),
            )
    }
}
