//! Bridge between GPUI events and Stoat editor engine.
//!
//! This module handles the translation layer between GPUI's event system
//! and Stoat's pure functional event processing. It ensures clean separation
//! between the UI layer and the editor logic.

use gpui::{Keystroke, Modifiers, MouseButton, Point};
use iced::keyboard;
use stoat::{EditorEngine, EditorEvent, Effect};

/// Converts GPUI keystrokes to Stoat editor events.
pub fn keystroke_to_event(keystroke: &Keystroke) -> Option<EditorEvent> {
    // Convert GPUI modifiers to iced modifiers
    let modifiers = convert_modifiers(&keystroke.modifiers);

    // Convert key string to iced key
    let key = convert_key(&keystroke.key)?;

    Some(EditorEvent::KeyPress { key, modifiers })
}

/// Converts GPUI modifiers to iced keyboard modifiers.
fn convert_modifiers(gpui_mods: &Modifiers) -> keyboard::Modifiers {
    let mut mods = keyboard::Modifiers::empty();

    if gpui_mods.control {
        mods |= keyboard::Modifiers::CTRL;
    }
    if gpui_mods.alt {
        mods |= keyboard::Modifiers::ALT;
    }
    if gpui_mods.shift {
        mods |= keyboard::Modifiers::SHIFT;
    }
    if gpui_mods.platform {
        // On macOS, command is the primary modifier (like Ctrl on other platforms)
        #[cfg(target_os = "macos")]
        {
            mods |= keyboard::Modifiers::LOGO;
        }
        #[cfg(not(target_os = "macos"))]
        {
            mods |= keyboard::Modifiers::CTRL;
        }
    }

    mods
}

/// Converts GPUI key string to iced keyboard key.
fn convert_key(key_str: &str) -> Option<keyboard::Key> {
    Some(match key_str {
        // Special keys
        "escape" | "esc" => keyboard::Key::Named(keyboard::key::Named::Escape),
        "enter" | "return" => keyboard::Key::Named(keyboard::key::Named::Enter),
        "tab" => keyboard::Key::Named(keyboard::key::Named::Tab),
        "backspace" => keyboard::Key::Named(keyboard::key::Named::Backspace),
        "delete" => keyboard::Key::Named(keyboard::key::Named::Delete),
        "space" => keyboard::Key::Named(keyboard::key::Named::Space),

        // Arrow keys
        "up" => keyboard::Key::Named(keyboard::key::Named::ArrowUp),
        "down" => keyboard::Key::Named(keyboard::key::Named::ArrowDown),
        "left" => keyboard::Key::Named(keyboard::key::Named::ArrowLeft),
        "right" => keyboard::Key::Named(keyboard::key::Named::ArrowRight),

        // Navigation keys
        "home" => keyboard::Key::Named(keyboard::key::Named::Home),
        "end" => keyboard::Key::Named(keyboard::key::Named::End),
        "pageup" => keyboard::Key::Named(keyboard::key::Named::PageUp),
        "pagedown" => keyboard::Key::Named(keyboard::key::Named::PageDown),

        // Function keys
        "f1" => keyboard::Key::Named(keyboard::key::Named::F1),
        "f2" => keyboard::Key::Named(keyboard::key::Named::F2),
        "f3" => keyboard::Key::Named(keyboard::key::Named::F3),
        "f4" => keyboard::Key::Named(keyboard::key::Named::F4),
        "f5" => keyboard::Key::Named(keyboard::key::Named::F5),
        "f6" => keyboard::Key::Named(keyboard::key::Named::F6),
        "f7" => keyboard::Key::Named(keyboard::key::Named::F7),
        "f8" => keyboard::Key::Named(keyboard::key::Named::F8),
        "f9" => keyboard::Key::Named(keyboard::key::Named::F9),
        "f10" => keyboard::Key::Named(keyboard::key::Named::F10),
        "f11" => keyboard::Key::Named(keyboard::key::Named::F11),
        "f12" => keyboard::Key::Named(keyboard::key::Named::F12),

        // Regular characters (including shifted ones)
        s if s.len() == 1 => keyboard::Key::Character(s.into()),

        // Multi-character strings or unknown keys
        _ => {
            tracing::warn!("Unknown key: {}", key_str);
            return None;
        },
    })
}

/// Converts GPUI mouse click to Stoat editor event.
pub fn mouse_click_to_event(position: Point<f32>, button: MouseButton) -> EditorEvent {
    EditorEvent::MouseClick {
        position: iced::Point::new(position.x, position.y),
        button: convert_mouse_button(button),
    }
}

/// Converts GPUI mouse button to iced mouse button.
fn convert_mouse_button(button: MouseButton) -> iced::mouse::Button {
    match button {
        MouseButton::Left => iced::mouse::Button::Left,
        MouseButton::Right => iced::mouse::Button::Right,
        MouseButton::Middle => iced::mouse::Button::Middle,
        MouseButton::Navigate(gpui::NavigationDirection::Back) => iced::mouse::Button::Back,
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => iced::mouse::Button::Forward,
    }
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

    /// Handles a mouse click and returns effects to process.
    pub fn handle_mouse_click(&mut self, position: Point<f32>, button: MouseButton) -> Vec<Effect> {
        let event = mouse_click_to_event(position, button);
        self.engine.handle_event(event)
    }

    /// Returns the current text content.
    pub fn text(&self) -> String {
        self.engine.text()
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
