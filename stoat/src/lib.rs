pub mod actions;
mod cursor;
pub mod keymap;
pub mod log;
pub mod pane;
mod scroll;
mod selection;

#[cfg(test)]
pub mod stoat_test;

/// Editor mode definition
///
/// A mode represents a distinct editing context with its own set of keybindings.
/// Modes are defined at runtime, allowing for user-configurable modal editing behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Internal identifier for the mode (e.g., "normal", "insert")
    pub name: String,
    /// Display name shown to users (e.g., "NORMAL", "INSERT")
    pub display_name: String,
}

impl Mode {
    /// Create a new mode with the given name and display name
    pub fn new(name: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
        }
    }
}

// Re-export types that the GUI layer will need
use cursor::CursorManager;
pub use cursor::{Cursor, CursorManager as PublicCursorManager};
use gpui::{App, AppContext, Entity};
pub use scroll::{ScrollDelta, ScrollPosition};
use std::num::NonZeroU64;
use stoat_rope_v3::{TokenMap, TokenSnapshot};
use stoat_text_v3::{Language, Parser};
use text::{Buffer, BufferId, BufferSnapshot, Point};

#[derive(Clone)]
pub struct Stoat {
    buffer: Entity<Buffer>,
    token_map: TokenMap,
    parser: Parser,
    current_language: Language,
    scroll: ScrollPosition,
    cursor_manager: CursorManager,
    viewport_lines: Option<f32>,
    modes: Vec<Mode>,
    current_mode: String,
}

impl Stoat {
    pub fn new(cx: &mut App) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_map = TokenMap::new(&buffer_snapshot);

        let current_language = Language::PlainText;
        let parser = Parser::new(current_language).expect("Failed to create parser");

        Self {
            buffer,
            token_map,
            parser,
            current_language,
            scroll: ScrollPosition::new(),
            cursor_manager: CursorManager::new(),
            viewport_lines: None,
            modes: vec![
                Mode::new("normal", "NORMAL"),
                Mode::new("insert", "INSERT"),
                Mode::new("visual", "VISUAL"),
                Mode::new("pane", "PANE"),
            ],
            current_mode: "normal".into(),
        }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.buffer.read(cx).snapshot()
    }

    pub fn token_snapshot(&self) -> TokenSnapshot {
        self.token_map.snapshot()
    }

    pub fn scroll_position(&self) -> gpui::Point<f32> {
        self.scroll.position
    }

    pub fn load_files(&mut self, paths: &[&std::path::Path], cx: &mut App) {
        // Load first file into buffer
        if let Some(first_path) = paths.first() {
            if let Ok(contents) = std::fs::read_to_string(first_path) {
                // Detect language from file extension
                let language = first_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(Language::from_extension)
                    .unwrap_or(Language::PlainText);

                // Update parser if language changed
                if language != self.current_language {
                    self.current_language = language;
                    self.parser = Parser::new(language).expect("Failed to create parser");
                }

                // Update buffer
                self.buffer.update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, contents.as_str())]);
                });

                // Parse and update tokens
                let buffer_snapshot = self.buffer.read(cx).snapshot();
                match self.parser.parse(&contents, &buffer_snapshot) {
                    Ok(tokens) => {
                        self.token_map.replace_tokens(tokens, &buffer_snapshot);
                    },
                    Err(e) => {
                        tracing::error!("Failed to parse file: {}", e);
                    },
                }
            }
        }
    }

    pub fn buffer_contents(&self, cx: &App) -> String {
        self.buffer.read(cx).text()
    }

    /// Get the current cursor position
    pub fn cursor_position(&self) -> Point {
        self.cursor_manager.position()
    }

    /// Set the cursor position
    pub fn set_cursor_position(&mut self, position: Point) {
        self.cursor_manager.move_to(position);
    }

    /// Get the visible line count (viewport height in lines)
    pub fn visible_line_count(&self) -> Option<f32> {
        self.viewport_lines
    }

    /// Set the visible line count based on viewport dimensions
    pub fn set_visible_line_count(&mut self, lines: f32) {
        self.viewport_lines = Some(lines);
    }

    /// Get the current editor mode name
    pub fn mode(&self) -> &str {
        &self.current_mode
    }

    /// Set the editor mode by name
    pub fn set_mode(&mut self, mode: &str) {
        self.current_mode = mode.to_string();
    }

    /// Get all available modes
    pub fn modes(&self) -> &[Mode] {
        &self.modes
    }

    /// Get a specific mode by name
    pub fn get_mode(&self, name: &str) -> Option<&Mode> {
        self.modes.iter().find(|m| m.name == name)
    }

    /// Get the cursor manager
    pub fn cursor_manager(&self) -> &CursorManager {
        &self.cursor_manager
    }

    /// Get the cursor manager mutably
    pub fn cursor_manager_mut(&mut self) -> &mut CursorManager {
        &mut self.cursor_manager
    }

    /// Update scroll animation and return true if still animating
    pub fn update_scroll_animation(&mut self) -> bool {
        !self.scroll.update_animation()
    }

    /// Check if scroll animation is in progress
    pub fn is_scroll_animating(&self) -> bool {
        self.scroll.is_animating()
    }

    /// Ensure the cursor is visible within the viewport by adjusting scroll position if needed.
    ///
    /// This method checks if the current cursor position is outside the visible viewport and
    /// animates the scroll position to bring it into view. It maintains some padding (3 lines)
    /// from the viewport edges for better context visibility.
    ///
    /// # Behavior
    ///
    /// - If cursor is above viewport: scrolls up to show cursor 3 lines from bottom
    /// - If cursor is below viewport: scrolls down to show cursor 3 lines from top
    /// - If cursor is within viewport: no scroll adjustment
    /// - Does nothing if viewport dimensions are not set
    ///
    /// # Padding
    ///
    /// The 3-line padding provides context around the cursor position and prevents it from
    /// appearing at the very edge of the viewport, which improves readability.
    ///
    /// # Related
    ///
    /// This is called automatically by movement commands after cursor position changes.
    /// See [`crate::actions::movement`] for usage examples.
    pub fn ensure_cursor_visible(&mut self) {
        // Get viewport dimensions - if not set, we can't determine visibility
        let Some(viewport_lines) = self.viewport_lines else {
            return;
        };

        let cursor_row = self.cursor_manager.position().row as f32;
        let scroll_y = self.scroll.position.y;
        let last_visible_line = scroll_y + viewport_lines;

        const PADDING: f32 = 3.0; // Lines of padding from viewport edges

        // Check if cursor is above viewport
        if cursor_row < scroll_y {
            // Scroll up to show cursor near bottom of viewport
            let target_scroll_y = (cursor_row - viewport_lines + PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
        // Check if cursor is below viewport
        else if cursor_row >= last_visible_line {
            // Scroll down to show cursor near top of viewport
            let target_scroll_y = (cursor_row - PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
        // Cursor is within viewport - no adjustment needed
    }
}

pub mod cli {
    pub mod config {
        use clap::Parser;

        #[derive(Parser)]
        #[command(name = "stoat")]
        #[command(about = "A text editor", long_about = None)]
        pub struct Cli {
            #[command(subcommand)]
            pub command: Option<Command>,
        }

        #[derive(Parser)]
        pub enum Command {
            #[cfg(feature = "gui")]
            #[command(about = "Launch the graphical user interface")]
            Gui {
                #[arg(help = "Files to open")]
                paths: Vec<std::path::PathBuf>,

                #[arg(short, long, help = "Input sequence to execute")]
                input: Option<String>,
            },
        }
    }
}
