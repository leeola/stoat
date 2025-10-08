//! Core Stoat editor entity with Context<Self> pattern.
//!
//! This follows Zed's Buffer architecture - Stoat is an Entity that can spawn
//! self-updating async tasks.

use crate::{
    buffer_item::BufferItem,
    cursor::CursorManager,
    file_finder::PreviewData,
    scroll::ScrollPosition,
    worktree::{Entry, Worktree},
};
use gpui::{AppContext, Context, Entity, EventEmitter, Task};
use nucleo_matcher::{Config, Matcher};
use parking_lot::Mutex;
use std::{num::NonZeroU64, path::PathBuf, sync::Arc};
use stoat_text::Language;
use text::{Buffer, BufferId, Point};

/// Command information for command palette.
///
/// Contains metadata about an action that can be executed through the command palette.
/// Used for fuzzy searching and dynamic dispatch via [`std::any::TypeId`].
#[derive(Clone, Debug)]
pub struct CommandInfo {
    /// Action name (e.g., "MoveLeft", "Save")
    pub name: String,
    /// Description of what the command does
    pub description: String,
    /// TypeId for dispatching the action
    pub type_id: std::any::TypeId,
}

/// Events emitted by Stoat
#[derive(Clone, Debug)]
pub enum StoatEvent {
    /// Editor content or state changed
    Changed,
}

/// Main editor entity.
///
/// Key difference from old stoat: methods take `&mut Context<Self>` instead of `&mut App`.
/// This enables spawning self-updating async tasks.
pub struct Stoat {
    /// Active buffer item
    pub(crate) buffer_item: Entity<BufferItem>,

    /// Cursor position management
    pub(crate) cursor: CursorManager,

    /// Scroll position with animation
    pub(crate) scroll: ScrollPosition,

    /// Viewport height in lines
    pub(crate) viewport_lines: Option<f32>,

    /// Current mode (normal, insert, file_finder)
    pub(crate) mode: String,

    // File finder state
    pub(crate) file_finder_input: Option<Entity<Buffer>>,
    pub(crate) file_finder_files: Vec<Entry>,
    pub(crate) file_finder_filtered: Vec<PathBuf>,
    pub(crate) file_finder_selected: usize,
    pub(crate) file_finder_previous_mode: Option<String>,
    pub(crate) file_finder_preview: Option<PreviewData>,
    pub(crate) file_finder_preview_task: Option<Task<()>>,
    pub(crate) file_finder_matcher: Matcher,

    // Command palette state
    pub(crate) command_palette_input: Option<Entity<Buffer>>,
    pub(crate) command_palette_commands: Vec<CommandInfo>,
    pub(crate) command_palette_filtered: Vec<CommandInfo>,
    pub(crate) command_palette_selected: usize,
    pub(crate) command_palette_previous_mode: Option<String>,

    /// Worktree for file scanning
    pub(crate) worktree: Arc<Mutex<Worktree>>,
}

impl EventEmitter<StoatEvent> for Stoat {}

impl Stoat {
    /// Create new Stoat entity.
    ///
    /// Takes `&mut Context<Self>` to follow Zed's Buffer pattern.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let welcome_text = "Welcome to Stoat v4!\n\nPress 'i' to enter insert mode.\nType some text.\nPress Esc to return to normal mode.\n\nPress 'h', 'j', 'k', 'l' to move in normal mode.";
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, welcome_text));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, Language::PlainText, cx));

        let worktree = Arc::new(Mutex::new(Worktree::new(PathBuf::from("."))));

        Self {
            buffer_item,
            cursor: CursorManager::new(),
            scroll: ScrollPosition::new(),
            viewport_lines: None,
            mode: "normal".into(),
            file_finder_input: None,
            file_finder_files: Vec::new(),
            file_finder_filtered: Vec::new(),
            file_finder_selected: 0,
            file_finder_previous_mode: None,
            file_finder_preview: None,
            file_finder_preview_task: None,
            file_finder_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            command_palette_input: None,
            command_palette_commands: Vec::new(),
            command_palette_filtered: Vec::new(),
            command_palette_selected: 0,
            command_palette_previous_mode: None,
            worktree,
        }
    }

    /// Get buffer item entity (caller can access buffer via this)
    pub fn buffer_item(&self) -> &Entity<BufferItem> {
        &self.buffer_item
    }

    /// Get cursor position
    pub fn cursor_position(&self) -> Point {
        self.cursor.position()
    }

    /// Set cursor position
    pub fn set_cursor_position(&mut self, position: Point) {
        self.cursor.move_to(position);
    }

    /// Get current selection
    pub fn selection(&self) -> &crate::cursor::Selection {
        self.cursor.selection()
    }

    /// Get scroll position
    pub fn scroll_position(&self) -> gpui::Point<f32> {
        self.scroll.position
    }

    /// Get current mode
    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// Set mode
    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
    }

    /// Set viewport height in lines
    pub fn set_viewport_lines(&mut self, lines: f32) {
        self.viewport_lines = Some(lines);
    }

    /// Update scroll animation
    pub fn update_scroll_animation(&mut self) -> bool {
        !self.scroll.update_animation()
    }

    /// Check if scrolling
    pub fn is_scroll_animating(&self) -> bool {
        self.scroll.is_animating()
    }

    /// Ensure cursor is visible
    pub fn ensure_cursor_visible(&mut self) {
        let Some(viewport_lines) = self.viewport_lines else {
            return;
        };

        let cursor_row = self.cursor.position().row as f32;
        let scroll_y = self.scroll.position.y;
        let last_visible_line = scroll_y + viewport_lines;

        const PADDING: f32 = 3.0;

        if cursor_row < scroll_y {
            let target_scroll_y = (cursor_row - viewport_lines + PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else if cursor_row >= last_visible_line {
            let target_scroll_y = (cursor_row - PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }

    /// Load a file into the buffer.
    ///
    /// Reads file content, detects language from extension, updates buffer,
    /// and reparses for syntax highlighting.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to file to load
    /// * `cx` - GPUI context
    ///
    /// # Errors
    ///
    /// Returns error if file cannot be read.
    pub fn load_file(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;

        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(stoat_text::Language::from_extension)
            .unwrap_or(stoat_text::Language::PlainText);

        self.buffer_item.update(cx, |item, cx| {
            item.set_language(language);
            item.buffer().update(cx, |buffer, _| {
                let len = buffer.len();
                buffer.edit([(0..len, contents.as_str())]);
            });
            let _ = item.reparse(cx);
        });

        self.cursor.move_to(text::Point::new(0, 0));
        cx.notify();

        Ok(())
    }

    /// Create a Stoat instance for testing with an empty buffer.
    ///
    /// Returns a [`TestStoat`] wrapper that provides test-oriented helper methods.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stoat = Stoat::test(cx);
    /// stoat.update(|s, cx| s.insert_text("hello", cx));
    /// assert_eq!(stoat.buffer_text(), "hello");
    /// ```
    #[cfg(test)]
    pub fn test(cx: &mut gpui::TestAppContext) -> crate::test::TestStoat<'_> {
        crate::test::TestStoat::new("", cx)
    }

    /// Create a Stoat instance for testing with specific buffer content.
    ///
    /// Returns a [`TestStoat`] wrapper that provides test-oriented helper methods.
    ///
    /// # Arguments
    ///
    /// * `text` - Initial buffer content
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stoat = Stoat::test_with_text("hello\nworld", cx);
    /// assert_eq!(stoat.buffer_text(), "hello\nworld");
    /// ```
    #[cfg(test)]
    pub fn test_with_text<'a>(
        text: &str,
        cx: &'a mut gpui::TestAppContext,
    ) -> crate::test::TestStoat<'a> {
        crate::test::TestStoat::new(text, cx)
    }
}
