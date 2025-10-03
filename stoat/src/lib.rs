pub mod actions;
mod cursor;
pub mod keymap;
pub mod log;
pub mod pane;
mod scroll;
mod selection;

#[cfg(test)]
pub mod stoat_test;

/// Editor modes for modal editing
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorMode {
    /// Normal mode - for navigation and commands
    #[default]
    Normal,
    /// Insert mode - for text input
    Insert,
    /// Visual mode - for text selection
    Visual,
    /// Pane mode - for pane management commands
    Pane,
}

impl EditorMode {
    /// Returns the string representation of the mode for key binding predicates
    pub fn as_str(&self) -> &'static str {
        match self {
            EditorMode::Normal => "normal",
            EditorMode::Insert => "insert",
            EditorMode::Visual => "visual",
            EditorMode::Pane => "pane",
        }
    }

    /// Returns the string representation of the mode for display
    pub fn as_display_str(&self) -> &'static str {
        match self {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Visual => "VISUAL",
            EditorMode::Pane => "PANE",
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
    mode: EditorMode,
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
            mode: EditorMode::default(),
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

    /// Get the current editor mode
    pub fn mode(&self) -> EditorMode {
        self.mode
    }

    /// Set the editor mode
    pub fn set_mode(&mut self, mode: EditorMode) {
        self.mode = mode;
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
