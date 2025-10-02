pub mod actions;
mod cursor;
pub mod keymap;
pub mod log;
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
}

impl EditorMode {
    /// Returns the string representation of the mode for key binding predicates
    pub fn as_str(&self) -> &'static str {
        match self {
            EditorMode::Normal => "normal",
            EditorMode::Insert => "insert",
            EditorMode::Visual => "visual",
        }
    }

    /// Returns the string representation of the mode for display
    pub fn as_display_str(&self) -> &'static str {
        match self {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Visual => "VISUAL",
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

    /// Insert text at the current cursor position
    pub fn insert_text(&mut self, text: &str, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let cursor_offset = buffer_snapshot.point_to_offset(self.cursor_manager.position());

        self.buffer.update(cx, |buffer, _cx| {
            buffer.edit([(cursor_offset..cursor_offset, text)]);
        });

        // Re-parse entire buffer after edit
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let contents = buffer_snapshot.text();
        match self.parser.parse(&contents, &buffer_snapshot) {
            Ok(tokens) => {
                self.token_map.replace_tokens(tokens, &buffer_snapshot);
            },
            Err(e) => {
                tracing::error!("Failed to parse after insert: {}", e);
            },
        }

        // Move cursor forward by the inserted text length
        let new_cursor_position = buffer_snapshot.offset_to_point(cursor_offset + text.len());
        self.cursor_manager.move_to(new_cursor_position);
    }

    /// Move cursor left by one character
    pub fn move_cursor_left(&mut self, cx: &App) {
        let current_pos = self.cursor_manager.position();
        if current_pos.column > 0 {
            let new_pos = Point::new(current_pos.row, current_pos.column - 1);
            self.cursor_manager.move_to(new_pos);
        } else if current_pos.row > 0 {
            // Move to end of previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = current_pos.row - 1;
            let line_len = buffer_snapshot.line_len(prev_row);
            let new_pos = Point::new(prev_row, line_len);
            self.cursor_manager.move_to(new_pos);
        }
    }

    /// Move cursor right by one character
    pub fn move_cursor_right(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);

        if current_pos.column < line_len {
            let new_pos = Point::new(current_pos.row, current_pos.column + 1);
            self.cursor_manager.move_to(new_pos);
        } else if current_pos.row < buffer_snapshot.row_count() - 1 {
            // Move to start of next line
            let new_pos = Point::new(current_pos.row + 1, 0);
            self.cursor_manager.move_to(new_pos);
        }
    }

    /// Move cursor up by one line
    pub fn move_cursor_up(&mut self, cx: &App) {
        let current_pos = self.cursor_manager.position();
        if current_pos.row > 0 {
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let new_row = current_pos.row - 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            self.cursor_manager.move_to_with_goal(new_pos);
        }
    }

    /// Move cursor down by one line
    pub fn move_cursor_down(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let max_row = buffer_snapshot.row_count() - 1;
        let current_pos = self.cursor_manager.position();

        if current_pos.row < max_row {
            let new_row = current_pos.row + 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            self.cursor_manager.move_to_with_goal(new_pos);
        }
    }

    /// Move cursor to start of current line
    pub fn move_cursor_to_line_start(&mut self) {
        let current_pos = self.cursor_manager.position();
        let new_pos = Point::new(current_pos.row, 0);
        self.cursor_manager.move_to(new_pos);
    }

    /// Move cursor to end of current line
    pub fn move_cursor_to_line_end(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);
        let new_pos = Point::new(current_pos.row, line_len);
        self.cursor_manager.move_to(new_pos);
    }

    /// Move cursor to start of file
    pub fn move_cursor_to_file_start(&mut self) {
        let new_pos = Point::new(0, 0);
        self.cursor_manager.move_to(new_pos);
    }

    /// Move cursor to end of file
    pub fn move_cursor_to_file_end(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        let new_pos = Point::new(last_row, last_line_len);
        self.cursor_manager.move_to(new_pos);
    }

    /// Delete character to the left of cursor (backspace)
    pub fn delete_left(&mut self, cx: &mut App) {
        let current_pos = self.cursor_manager.position();
        if current_pos.column > 0 {
            // Delete character on same line
            let start = Point::new(current_pos.row, current_pos.column - 1);
            let end = current_pos;
            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        } else if current_pos.row > 0 {
            // Merge with previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = current_pos.row - 1;
            let prev_line_len = buffer_snapshot.line_len(prev_row);
            let start = Point::new(prev_row, prev_line_len);
            let end = Point::new(current_pos.row, 0);

            self.delete_range(start..end, cx);
            self.cursor_manager.move_to(start);
        }
    }

    /// Delete character to the right of cursor (delete)
    pub fn delete_right(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);

        if current_pos.column < line_len {
            // Delete character on same line
            let start = current_pos;
            let end = Point::new(current_pos.row, current_pos.column + 1);
            self.delete_range(start..end, cx);
        } else if current_pos.row < buffer_snapshot.row_count() - 1 {
            // Merge with next line
            let start = current_pos;
            let end = Point::new(current_pos.row + 1, 0);
            self.delete_range(start..end, cx);
        }
    }

    /// Delete the current line
    pub fn delete_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_start = Point::new(current_pos.row, 0);

        // Include newline if not last line
        let line_end = if current_pos.row < buffer_snapshot.row_count() - 1 {
            Point::new(current_pos.row + 1, 0)
        } else {
            // Last line - delete to end of line
            let line_len = buffer_snapshot.line_len(current_pos.row);
            Point::new(current_pos.row, line_len)
        };

        self.delete_range(line_start..line_end, cx);
        self.cursor_manager.move_to(line_start);
    }

    /// Delete from cursor to end of line
    pub fn delete_to_end_of_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();
        let line_len = buffer_snapshot.line_len(current_pos.row);
        let end = Point::new(current_pos.row, line_len);

        if current_pos.column < line_len {
            self.delete_range(current_pos..end, cx);
        }
    }

    /// Move cursor up by one page (approximately one viewport height)
    pub fn move_cursor_page_up(&mut self, cx: &App) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let current_pos = self.cursor_manager.position();

        if lines_per_page > 0 {
            let new_row = current_pos.row.saturating_sub(lines_per_page);
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            self.cursor_manager.move_to_with_goal(new_pos);

            // Start animated scroll to keep the cursor visible
            let target_scroll_y = new_row.saturating_sub(3) as f32;
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }

    /// Move cursor down by one page (approximately one viewport height)
    pub fn move_cursor_page_down(&mut self, cx: &App) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let max_row = buffer_snapshot.row_count() - 1;
        let current_pos = self.cursor_manager.position();

        if lines_per_page > 0 && max_row > 0 {
            let new_row = (current_pos.row + lines_per_page).min(max_row);

            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_manager.goal_column().min(line_len);
            let new_pos = Point::new(new_row, new_column);
            self.cursor_manager.move_to_with_goal(new_pos);

            // Start animated scroll to keep the cursor visible
            let target_scroll_y = new_row.saturating_sub(3) as f32;
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }

    /// Update scroll animation and return true if still animating
    pub fn update_scroll_animation(&mut self) -> bool {
        !self.scroll.update_animation()
    }

    /// Check if scroll animation is in progress
    pub fn is_scroll_animating(&self) -> bool {
        self.scroll.is_animating()
    }

    /// Helper method to delete a range of text
    fn delete_range(&mut self, range: std::ops::Range<Point>, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let start_offset = buffer_snapshot.point_to_offset(range.start);
        let end_offset = buffer_snapshot.point_to_offset(range.end);

        if start_offset < end_offset {
            self.buffer.update(cx, |buffer, _cx| {
                buffer.edit([(start_offset..end_offset, "")]);
            });

            // Re-parse entire buffer after deletion
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let contents = buffer_snapshot.text();
            match self.parser.parse(&contents, &buffer_snapshot) {
                Ok(tokens) => {
                    self.token_map.replace_tokens(tokens, &buffer_snapshot);
                },
                Err(e) => {
                    tracing::error!("Failed to parse after delete: {}", e);
                },
            }
        }
    }

    /// Handle scroll events from mouse wheel or trackpad
    ///
    /// This method processes scroll deltas and updates the viewport position with
    /// configurable sensitivity and support for fast scrolling.
    pub fn handle_scroll_event(&mut self, delta: &ScrollDelta, fast_scroll: bool, cx: &App) {
        // Default scroll sensitivity values (similar to Zed)
        let base_sensitivity = 1.0;
        let fast_multiplier = 3.0;

        // Get line height for delta conversion
        // FIXME: Use actual line height from style or calculate dynamically
        let line_height = 20.0;

        // Calculate new scroll position
        let new_position = self.scroll.apply_scroll_delta(
            delta,
            line_height,
            base_sensitivity,
            fast_multiplier,
            fast_scroll,
        );

        // Apply bounds checking
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let max_scroll_y = (buffer_snapshot.row_count() as f32 - 1.0).max(0.0);

        let bounded_position = gpui::point(
            new_position.x.max(0.0),                   // No negative horizontal scroll
            new_position.y.max(0.0).min(max_scroll_y), // Clamp vertical scroll
        );

        // Update scroll position
        self.scroll.scroll_to(bounded_position);
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
