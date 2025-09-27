pub mod log;
mod scroll;

use gpui::{App, AppContext, Entity};
pub use scroll::ScrollPosition;
use std::num::NonZeroU64;
use stoat_rope_v3::{TokenMap, TokenSnapshot};
use text::{Buffer, BufferId, BufferSnapshot, Point};

#[derive(Clone)]
pub struct Stoat {
    buffer: Entity<Buffer>,
    token_map: TokenMap,
    scroll: ScrollPosition,
    cursor_position: Point,
}

impl Stoat {
    pub fn new(cx: &mut App) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_map = TokenMap::new(&buffer_snapshot);

        Self {
            buffer,
            token_map,
            scroll: ScrollPosition::new(),
            cursor_position: Point::new(0, 0),
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
                self.buffer.update(cx, |buffer, _| {
                    // Clear existing content and insert new
                    let len = buffer.len();
                    buffer.edit([(0..len, contents.as_str())]);
                });

                // Sync token map with new buffer content
                let buffer_snapshot = self.buffer.read(cx).snapshot();
                // For initial load, we can simulate an edit of the entire buffer
                let edit = text::Edit {
                    old: 0..0,
                    new: 0..buffer_snapshot.len(),
                };
                self.token_map.sync(&buffer_snapshot, &[edit]);
            }
        }
    }

    pub fn buffer_contents(&self, cx: &App) -> String {
        self.buffer.read(cx).text()
    }

    /// Get the current cursor position
    pub fn cursor_position(&self) -> Point {
        self.cursor_position
    }

    /// Set the cursor position
    pub fn set_cursor_position(&mut self, position: Point) {
        self.cursor_position = position;
    }

    /// Insert text at the current cursor position
    pub fn insert_text(&mut self, text: &str, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let cursor_offset = buffer_snapshot.point_to_offset(self.cursor_position);

        self.buffer.update(cx, |buffer, _cx| {
            buffer.edit([(cursor_offset..cursor_offset, text)]);
        });

        // Sync token map with the edit
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let edit = text::Edit {
            old: cursor_offset..cursor_offset,
            new: cursor_offset..(cursor_offset + text.len()),
        };
        self.token_map.sync(&buffer_snapshot, &[edit]);

        // Move cursor forward by the inserted text length
        let new_cursor_position = buffer_snapshot.offset_to_point(cursor_offset + text.len());
        self.cursor_position = new_cursor_position;
    }

    /// Move cursor left by one character
    pub fn move_cursor_left(&mut self, cx: &App) {
        if self.cursor_position.column > 0 {
            self.cursor_position.column -= 1;
        } else if self.cursor_position.row > 0 {
            // Move to end of previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = self.cursor_position.row - 1;
            let line_len = buffer_snapshot.line_len(prev_row);
            self.cursor_position = Point::new(prev_row, line_len);
        }
    }

    /// Move cursor right by one character
    pub fn move_cursor_right(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let line_len = buffer_snapshot.line_len(self.cursor_position.row);

        if self.cursor_position.column < line_len {
            self.cursor_position.column += 1;
        } else if self.cursor_position.row < buffer_snapshot.row_count() - 1 {
            // Move to start of next line
            self.cursor_position = Point::new(self.cursor_position.row + 1, 0);
        }
    }

    /// Move cursor up by one line
    pub fn move_cursor_up(&mut self, cx: &App) {
        if self.cursor_position.row > 0 {
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let new_row = self.cursor_position.row - 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_position.column.min(line_len);
            self.cursor_position = Point::new(new_row, new_column);
        }
    }

    /// Move cursor down by one line
    pub fn move_cursor_down(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let max_row = buffer_snapshot.row_count() - 1;

        if self.cursor_position.row < max_row {
            let new_row = self.cursor_position.row + 1;
            let line_len = buffer_snapshot.line_len(new_row);
            let new_column = self.cursor_position.column.min(line_len);
            self.cursor_position = Point::new(new_row, new_column);
        }
    }

    /// Move cursor to start of current line
    pub fn move_cursor_to_line_start(&mut self) {
        self.cursor_position.column = 0;
    }

    /// Move cursor to end of current line
    pub fn move_cursor_to_line_end(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let line_len = buffer_snapshot.line_len(self.cursor_position.row);
        self.cursor_position.column = line_len;
    }

    /// Move cursor to start of file
    pub fn move_cursor_to_file_start(&mut self) {
        self.cursor_position = Point::new(0, 0);
    }

    /// Move cursor to end of file
    pub fn move_cursor_to_file_end(&mut self, cx: &App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        self.cursor_position = Point::new(last_row, last_line_len);
    }

    /// Delete character to the left of cursor (backspace)
    pub fn delete_left(&mut self, cx: &mut App) {
        if self.cursor_position.column > 0 {
            // Delete character on same line
            let start = Point::new(self.cursor_position.row, self.cursor_position.column - 1);
            let end = self.cursor_position;
            self.delete_range(start..end, cx);
            self.cursor_position = start;
        } else if self.cursor_position.row > 0 {
            // Merge with previous line
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let prev_row = self.cursor_position.row - 1;
            let prev_line_len = buffer_snapshot.line_len(prev_row);
            let start = Point::new(prev_row, prev_line_len);
            let end = Point::new(self.cursor_position.row, 0);

            self.delete_range(start..end, cx);
            self.cursor_position = start;
        }
    }

    /// Delete character to the right of cursor (delete)
    pub fn delete_right(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let line_len = buffer_snapshot.line_len(self.cursor_position.row);

        if self.cursor_position.column < line_len {
            // Delete character on same line
            let start = self.cursor_position;
            let end = Point::new(self.cursor_position.row, self.cursor_position.column + 1);
            self.delete_range(start..end, cx);
        } else if self.cursor_position.row < buffer_snapshot.row_count() - 1 {
            // Merge with next line
            let start = self.cursor_position;
            let end = Point::new(self.cursor_position.row + 1, 0);
            self.delete_range(start..end, cx);
        }
    }

    /// Delete the current line
    pub fn delete_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let line_start = Point::new(self.cursor_position.row, 0);

        // Include newline if not last line
        let line_end = if self.cursor_position.row < buffer_snapshot.row_count() - 1 {
            Point::new(self.cursor_position.row + 1, 0)
        } else {
            // Last line - delete to end of line
            let line_len = buffer_snapshot.line_len(self.cursor_position.row);
            Point::new(self.cursor_position.row, line_len)
        };

        self.delete_range(line_start..line_end, cx);
        self.cursor_position = line_start;
    }

    /// Delete from cursor to end of line
    pub fn delete_to_end_of_line(&mut self, cx: &mut App) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let line_len = buffer_snapshot.line_len(self.cursor_position.row);
        let end = Point::new(self.cursor_position.row, line_len);

        if self.cursor_position.column < line_len {
            self.delete_range(self.cursor_position..end, cx);
        }
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

            // Sync token map with the deletion
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let edit = text::Edit {
                old: start_offset..end_offset,
                new: start_offset..start_offset,
            };
            self.token_map.sync(&buffer_snapshot, &[edit]);
        }
    }
}

pub struct EditorEngine;

impl EditorEngine {
    pub fn new() -> Self {
        EditorEngine
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
