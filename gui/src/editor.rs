//! Simple keyboard input observer for GPUI

use crate::theme::EditorTheme;
use gpui::{
    div, prelude::*, App, Context, EventEmitter, FocusHandle, Focusable, Keystroke, ParentElement,
    Render, SharedString, Styled, Window,
};
use tracing::info;

/// Simple keyboard observer view for GPUI
pub struct Editor {
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
    /// Editor theme
    theme: EditorTheme,
    /// Last keystroke received
    last_keystroke: Option<Keystroke>,
}

impl Editor {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();
        info!("Editor created with focus handle");

        Self {
            focus_handle,
            theme: EditorTheme::default(),
            last_keystroke: None,
        }
    }

    /// Handle any keystroke
    pub fn on_keystroke(
        &mut self,
        keystroke: Keystroke,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        info!("Editor::on_keystroke called with: {:?}", keystroke);
        info!(
            "  Key: {:?}, Modifiers: {:?}",
            keystroke.key, keystroke.modifiers
        );
        self.last_keystroke = Some(keystroke.clone());
        cx.notify();

        info!(
            "Exiting application after keystroke: {}",
            keystroke.unparse()
        );
        // Exit the application on any key press
        std::process::exit(0);
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let message = if let Some(ref keystroke) = self.last_keystroke {
            format!("Last key: {}", keystroke.unparse())
        } else {
            "Press any key to log and exit...".to_string()
        };

        div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .bg(self.theme.background)
            .text_color(self.theme.foreground)
            .size_full()
            .font_family("JetBrains Mono")
            .flex()
            .items_center()
            .justify_center()
            .child(SharedString::from(message))
    }
}

impl EventEmitter<EditorEvent> for Editor {}

impl Focusable for Editor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Debug, Clone)]
pub enum EditorEvent {
    KeystrokeReceived(Keystroke),
}
