//! Bridge between GPUI events and Stoat editor engine.
//!
//! This module provides a thin layer between GPUI's event system
//! and Stoat's pure functional event processing. Since both now use
//! the same types, this bridge is greatly simplified.

use gpui::Keystroke;
use stoat::{EditorEngine, EditorEvent, Effect};

/// Converts GPUI keystrokes to Stoat editor events.
pub fn keystroke_to_event(keystroke: &Keystroke) -> Option<EditorEvent> {
    // Stoat now uses GPUI types directly, so conversion is straightforward
    Some(EditorEvent::KeyPress {
        key: keystroke.key.clone(),
        modifiers: keystroke.modifiers,
    })
}

/// Processes effects returned from Stoat engine.
///
/// This function handles side effects that Stoat delegates to the UI layer.
pub async fn process_effects(effects: Vec<Effect>) -> anyhow::Result<()> {
    for effect in effects {
        match effect {
            Effect::Exit => {
                tracing::info!("Exit effect received");
                std::process::exit(0);
            },

            Effect::ShowInfo { message } => {
                tracing::info!("Info: {}", message);
                // TODO: Show in UI status bar or notification
            },

            Effect::ShowError { message } => {
                tracing::error!("Error: {}", message);
                // TODO: Show error dialog or notification
            },

            Effect::SetTitle { title } => {
                tracing::info!("Setting window title: {}", title);
                // TODO: Update window title
            },

            Effect::Bell => {
                tracing::info!("Bell effect");
                // TODO: Ring terminal bell or visual bell
            },

            Effect::ShowHelp {
                visible,
                mode,
                commands,
            } => {
                tracing::info!(
                    "ShowHelp effect: visible={}, mode={}, {} commands",
                    visible,
                    mode,
                    commands.len()
                );
                // TODO: Update help dialog visibility and content
                // This will need to be handled by EditorView since it needs access to UI state
            },

            Effect::CommandContextChanged { mode, commands } => {
                tracing::info!(
                    "CommandContextChanged effect: mode={}, {} commands",
                    mode,
                    commands.len()
                );
                // TODO: Update command panel content
                // This will need to be handled by EditorView since it needs access to UI state
            },

            Effect::ViewportUpdate { scroll_x, scroll_y } => {
                tracing::info!(
                    "ViewportUpdate effect: scroll_x={}, scroll_y={}",
                    scroll_x,
                    scroll_y
                );
                // This will be handled by EditorView to update its viewport
            },
        }
    }

    Ok(())
}

/// Bridge state that maintains the connection between GPUI and Stoat.
pub struct StoatBridge {
    /// The Stoat editor engine
    pub engine: EditorEngine,
}

impl StoatBridge {
    /// Creates a new bridge with an empty editor.
    pub fn new() -> Self {
        Self {
            engine: EditorEngine::new(),
        }
    }

    /// Creates a new bridge with initial text content.
    pub fn with_text(text: &str) -> Self {
        Self {
            engine: EditorEngine::with_text(text),
        }
    }

    /// Handles a GPUI keystroke and returns effects to process.
    pub fn handle_keystroke(&mut self, keystroke: &Keystroke) -> Vec<Effect> {
        if let Some(event) = keystroke_to_event(keystroke) {
            self.engine.handle_event(event)
        } else {
            vec![]
        }
    }

    /// Returns the current cursor position.
    pub fn cursor_position(&self) -> (usize, usize) {
        let pos = self.engine.cursor_position();
        (pos.line, pos.column)
    }

    /// Returns the current editing mode.
    pub fn mode(&self) -> String {
        format!("{:?}", self.engine.mode())
    }

    /// Returns whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.engine.is_dirty()
    }
}

impl Default for StoatBridge {
    fn default() -> Self {
        Self::new()
    }
}
