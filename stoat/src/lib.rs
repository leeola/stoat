pub mod actions;
mod cursor;
pub mod keymap;
pub mod log;
pub mod pane;
mod scroll;
mod selection;
mod worktree;

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
use pane::BufferItem;
use parking_lot::Mutex;
pub use scroll::{ScrollDelta, ScrollPosition};
use std::{num::NonZeroU64, path::PathBuf, sync::Arc};
use stoat_rope_v3::TokenSnapshot;
use stoat_text_v3::Language;
use text::{Buffer, BufferId, BufferSnapshot, Point};
use worktree::Worktree;

#[derive(Clone)]
pub struct Stoat {
    /// Main buffer item (will become Vec<Box<dyn ItemHandle>> in next phase)
    item: Entity<BufferItem>,
    scroll: ScrollPosition,
    cursor_manager: CursorManager,
    viewport_lines: Option<f32>,
    modes: Vec<Mode>,
    current_mode: String,
    // File finder state
    file_finder_input: Option<Entity<Buffer>>,
    file_finder_files: Vec<PathBuf>,
    file_finder_filtered: Vec<PathBuf>,
    file_finder_selected: usize,
    file_finder_previous_mode: Option<String>,
    file_finder_preview: Option<String>,
    // Worktree (shared across cloned Stoats)
    worktree: Arc<Mutex<Worktree>>,
}

impl Stoat {
    pub fn new(cx: &mut App) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Create buffer item wrapping the buffer
        let item = cx.new(|cx| BufferItem::new(buffer, Language::PlainText, cx));

        // Initialize worktree for instant file finder
        let worktree = Arc::new(Mutex::new(Worktree::new(PathBuf::from("."))));

        Self {
            item,
            scroll: ScrollPosition::new(),
            cursor_manager: CursorManager::new(),
            viewport_lines: None,
            modes: crate::keymap::load_default_modes(),
            current_mode: "normal".into(),
            file_finder_input: None,
            file_finder_files: Vec::new(),
            file_finder_filtered: Vec::new(),
            file_finder_selected: 0,
            file_finder_previous_mode: None,
            file_finder_preview: None,
            worktree,
        }
    }

    pub fn buffer(&self, cx: &App) -> Entity<Buffer> {
        self.item.read(cx).buffer().clone()
    }

    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.item.read(cx).buffer_snapshot(cx)
    }

    pub fn token_snapshot(&self, cx: &App) -> TokenSnapshot {
        self.item.read(cx).token_snapshot()
    }

    pub fn scroll_position(&self) -> gpui::Point<f32> {
        self.scroll.position
    }

    pub fn load_files(&mut self, paths: &[&std::path::Path], cx: &mut App) {
        // Load first file into buffer item
        if let Some(first_path) = paths.first() {
            if let Ok(contents) = std::fs::read_to_string(first_path) {
                // Detect language from file extension
                let language = first_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(Language::from_extension)
                    .unwrap_or(Language::PlainText);

                // Update buffer item with new content and language
                self.item.update(cx, |item, cx| {
                    // Set language (updates parser if changed)
                    item.set_language(language);

                    // Update buffer content
                    item.buffer().update(cx, |buffer, _| {
                        let len = buffer.len();
                        buffer.edit([(0..len, contents.as_str())]);
                    });

                    // Reparse to update tokens
                    if let Err(e) = item.reparse(cx) {
                        tracing::error!("Failed to parse file: {}", e);
                    }
                });
            }
        }
    }

    pub fn buffer_contents(&self, cx: &App) -> String {
        self.item.read(cx).buffer().read(cx).text()
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

    /// Handle the SetMode action.
    ///
    /// Changes to the specified mode and logs the transition.
    /// This is the handler called by the GUI layer when a [`actions::SetMode`] action is
    /// dispatched.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`Self::set_mode`] - internal mode setter without logging
    /// - [`actions::SetMode`] - the action this handles
    pub fn handle_set_mode(&mut self, mode: &str) {
        tracing::debug!(to = mode, "Setting mode");
        self.set_mode(mode);
    }

    /// Get the file finder input buffer
    pub fn file_finder_input_buffer(&self) -> Option<&Entity<Buffer>> {
        self.file_finder_input.as_ref()
    }

    /// Get the file finder query text from the input buffer
    pub fn file_finder_query(&self, cx: &App) -> String {
        self.file_finder_input
            .as_ref()
            .map(|buf| buf.read(cx).snapshot().text())
            .unwrap_or_default()
    }

    /// Get the filtered files list
    pub fn file_finder_filtered_files(&self) -> &[PathBuf] {
        &self.file_finder_filtered
    }

    /// Get the selected file index
    pub fn file_finder_selected_index(&self) -> usize {
        self.file_finder_selected
    }

    /// Get the file preview content for the selected file
    pub fn file_finder_preview_content(&self) -> Option<&str> {
        self.file_finder_preview.as_deref()
    }

    /// Filter files based on the current query.
    ///
    /// Performs case-insensitive substring matching on file paths.
    /// Updates the filtered files list and resets selection to 0.
    pub fn filter_files(&mut self, query: &str) {
        if query.is_empty() {
            self.file_finder_filtered = self.file_finder_files.clone();
        } else {
            let query_lower = query.to_lowercase();
            self.file_finder_filtered = self
                .file_finder_files
                .iter()
                .filter(|path| path.to_string_lossy().to_lowercase().contains(&query_lower))
                .cloned()
                .collect();
        }

        // Reset selection to top when filter changes
        self.file_finder_selected = 0;
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
