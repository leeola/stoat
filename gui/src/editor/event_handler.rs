//! Input event processing for the text editor.
//!
//! This module handles keyboard and mouse events, converting them to
//! editor actions and managing text selection.

use super::{buffer::TextBuffer, layout::EditorLayout};
use iced::{
    event::Event,
    keyboard::{self, Key, Modifiers},
    mouse::{self, Button, ScrollDelta},
    Point,
};
use stoat::EditorEvent;

/// Handles input events for the text editor
pub struct EventHandler {
    /// Current modifiers state
    modifiers: Modifiers,
    /// Mouse dragging state
    dragging: Option<DragState>,
    /// Last click time for double/triple click detection
    last_click: Option<(std::time::Instant, ClickType)>,
    /// Accumulated scroll pixels (for smooth scrolling)
    scroll_accumulator: (f32, f32),
}

#[derive(Debug, Clone)]
enum DragState {
    /// Dragging for text selection
    Selection { start: Point },
    /// Dragging vertical scrollbar
    ScrollbarV { start_y: f32, initial_scroll: f32 },
    /// Dragging horizontal scrollbar
    ScrollbarH { start_x: f32, initial_scroll: f32 },
}

#[derive(Debug, Clone, Copy)]
enum ClickType {
    Single,
    Double,
    Triple,
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHandler {
    /// Creates a new event handler
    pub fn new() -> Self {
        Self {
            modifiers: Modifiers::empty(),
            dragging: None,
            last_click: None,
            scroll_accumulator: (0.0, 0.0),
        }
    }

    /// Processes an event and returns the appropriate editor event
    pub fn process_event(
        &mut self,
        event: Event,
        layout: &EditorLayout,
        buffer: &TextBuffer,
        cursor_position: Point,
    ) -> Option<EditorEvent> {
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => {
                self.modifiers = modifiers;
                self.handle_key_press(key, modifiers, text.map(|s| s.to_string()))
            },

            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                self.modifiers = modifiers;
                None
            },

            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                self.handle_mouse_press(button, cursor_position, layout, buffer)
            },

            Event::Mouse(mouse::Event::ButtonReleased(button)) => self.handle_mouse_release(button),

            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                self.handle_mouse_move(position, layout, buffer)
            },

            Event::Mouse(mouse::Event::WheelScrolled { delta }) => self.handle_scroll(delta),

            _ => None,
        }
    }

    /// Handles keyboard input
    fn handle_key_press(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        text: Option<String>,
    ) -> Option<EditorEvent> {
        // Handle text input with proper modifier handling
        match (&key, text) {
            // If it's a Character key and we have text, use the text
            // (this handles shifted chars like "?")
            (Key::Character(_), Some(text))
                if !text.is_empty()
                    && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo() =>
            {
                // Remove SHIFT since it's already applied in the text
                let mut effective_modifiers = modifiers;
                effective_modifiers.remove(Modifiers::SHIFT);

                Some(EditorEvent::KeyPress {
                    key: Key::Character(text.into()),
                    modifiers: effective_modifiers,
                })
            },
            // For everything else (Named keys, empty text, etc.), use the original
            _ => Some(EditorEvent::KeyPress {
                key: key.clone(),
                modifiers,
            }),
        }
    }

    /// Handles mouse button press
    fn handle_mouse_press(
        &mut self,
        button: Button,
        position: Point,
        layout: &EditorLayout,
        buffer: &TextBuffer,
    ) -> Option<EditorEvent> {
        if button != Button::Left {
            return Some(EditorEvent::MouseClick { position, button });
        }

        // Check if clicking on scrollbars
        if let Some(_v_scrollbar) = self.check_vertical_scrollbar_hit(position, layout, buffer) {
            self.dragging = Some(DragState::ScrollbarV {
                start_y: position.y,
                initial_scroll: layout.scroll_y,
            });
            return None;
        }

        if let Some(_h_scrollbar) = self.check_horizontal_scrollbar_hit(position, layout, buffer) {
            self.dragging = Some(DragState::ScrollbarH {
                start_x: position.x,
                initial_scroll: layout.scroll_x,
            });
            return None;
        }

        // Check for multi-click
        let click_type = self.detect_click_type();

        // Start text selection drag
        self.dragging = Some(DragState::Selection { start: position });

        // Convert click type to appropriate event
        match click_type {
            ClickType::Single => Some(EditorEvent::MouseClick { position, button }),
            ClickType::Double => Some(EditorEvent::MouseClick { position, button }),
            ClickType::Triple => Some(EditorEvent::MouseClick { position, button }),
        }
    }

    /// Handles mouse button release
    fn handle_mouse_release(&mut self, _button: Button) -> Option<EditorEvent> {
        self.dragging = None;
        None
    }

    /// Handles mouse movement
    fn handle_mouse_move(
        &mut self,
        position: Point,
        layout: &EditorLayout,
        _buffer: &TextBuffer,
    ) -> Option<EditorEvent> {
        match &self.dragging {
            Some(DragState::Selection { start: _ }) => {
                // Handle text selection dragging
                // For now, convert drag to click events
                Some(EditorEvent::MouseClick {
                    position,
                    button: Button::Left,
                })
            },

            Some(DragState::ScrollbarV {
                start_y,
                initial_scroll,
            }) => {
                // Handle vertical scrollbar dragging
                let delta_y = position.y - start_y;
                let new_scroll = initial_scroll + delta_y * 2.0; // Scale factor for sensitivity
                Some(EditorEvent::Scroll {
                    delta_x: 0.0,
                    delta_y: new_scroll - layout.scroll_y,
                })
            },

            Some(DragState::ScrollbarH {
                start_x,
                initial_scroll,
            }) => {
                // Handle horizontal scrollbar dragging
                let delta_x = position.x - start_x;
                let new_scroll = initial_scroll + delta_x * 2.0;
                Some(EditorEvent::Scroll {
                    delta_x: new_scroll - layout.scroll_x,
                    delta_y: 0.0,
                })
            },

            None => None,
        }
    }

    /// Handles scroll wheel events
    fn handle_scroll(&mut self, delta: ScrollDelta) -> Option<EditorEvent> {
        let (delta_x, delta_y) = match delta {
            ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
            ScrollDelta::Pixels { x, y } => (x, y),
        };

        // Accumulate small scroll amounts for smooth scrolling
        self.scroll_accumulator.0 += delta_x;
        self.scroll_accumulator.1 += delta_y;

        // Only send event if accumulated enough
        if self.scroll_accumulator.0.abs() > 1.0 || self.scroll_accumulator.1.abs() > 1.0 {
            let event = Some(EditorEvent::Scroll {
                delta_x: self.scroll_accumulator.0,
                delta_y: -self.scroll_accumulator.1, // Invert for natural scrolling
            });
            self.scroll_accumulator = (0.0, 0.0);
            event
        } else {
            None
        }
    }

    /// Detects click type (single, double, triple)
    fn detect_click_type(&mut self) -> ClickType {
        const DOUBLE_CLICK_TIME: std::time::Duration = std::time::Duration::from_millis(500);

        let now = std::time::Instant::now();

        let click_type = if let Some((last_time, last_type)) = self.last_click {
            if now.duration_since(last_time) < DOUBLE_CLICK_TIME {
                match last_type {
                    ClickType::Single => ClickType::Double,
                    ClickType::Double => ClickType::Triple,
                    ClickType::Triple => ClickType::Single,
                }
            } else {
                ClickType::Single
            }
        } else {
            ClickType::Single
        };

        self.last_click = Some((now, click_type));
        click_type
    }

    /// Checks if a point hits the vertical scrollbar
    fn check_vertical_scrollbar_hit(
        &self,
        position: Point,
        layout: &EditorLayout,
        buffer: &TextBuffer,
    ) -> Option<Rectangle> {
        let metrics = buffer.metrics();
        let (start_line, end_line) = layout.visible_line_range(metrics);
        let total_lines = buffer.line_count();

        let scrollbar = layout.vertical_scrollbar_bounds(start_line, end_line, total_lines);

        if scrollbar.contains(position) {
            Some(scrollbar)
        } else {
            None
        }
    }

    /// Checks if a point hits the horizontal scrollbar
    fn check_horizontal_scrollbar_hit(
        &self,
        position: Point,
        layout: &EditorLayout,
        buffer: &TextBuffer,
    ) -> Option<Rectangle> {
        if let Some(scrollbar) = layout.horizontal_scrollbar_bounds(buffer) {
            if scrollbar.contains(position) {
                return Some(scrollbar);
            }
        }
        None
    }
}

use iced::Rectangle; // Add this import for scrollbar hit detection
