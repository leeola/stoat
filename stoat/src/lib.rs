pub mod actions;
mod cursor;
pub mod git_diff;
pub mod git_repository;
pub mod keymap;
pub mod log;
pub mod pane;
mod rel_path;
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
use actions::shell::load_file_preview;
pub use actions::shell::PreviewData;
use cursor::CursorManager;
pub use cursor::{Cursor, CursorManager as PublicCursorManager};
use gpui::{App, AppContext, Entity};
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher,
};
use pane::{BufferItem, ItemVariant};
use parking_lot::Mutex;
pub use scroll::{ScrollDelta, ScrollPosition};
use std::{any::TypeId, num::NonZeroU64, path::PathBuf, sync::Arc};
use stoat_rope::TokenSnapshot;
use stoat_text::Language;
use text::{Buffer, BufferId, BufferSnapshot, Point};
use worktree::{Entry, Worktree};

/// Information about an available command for the command palette.
///
/// This struct holds metadata about an action that can be executed via the command palette.
/// Commands are built from the keymap bindings and displayed with fuzzy search.
#[derive(Clone, Debug)]
pub struct CommandInfo {
    /// Action name (e.g., "MoveLeft", "Save")
    pub name: String,
    /// Description of what the command does
    pub description: String,
    /// TypeId for dispatching the action
    pub type_id: TypeId,
}

pub struct Stoat {
    /// Items displayed in editor (buffers, terminals, etc.)
    ///
    /// Uses enum_dispatch for efficient static dispatch - 4-10x faster than trait objects.
    /// Items can be downcast via helper methods like `as_buffer()`.
    items: Vec<ItemVariant>,
    /// Index of currently active item
    active_item_index: usize,
    scroll: ScrollPosition,
    cursor_manager: CursorManager,
    viewport_lines: Option<f32>,
    modes: Vec<Mode>,
    current_mode: String,
    // File finder state
    file_finder_input: Option<Entity<Buffer>>,
    file_finder_files: Vec<Entry>,
    file_finder_filtered: Vec<PathBuf>,
    file_finder_selected: usize,
    file_finder_previous_mode: Option<String>,
    file_finder_preview: Option<PreviewData>,
    file_finder_matcher: Matcher,
    // Command palette state
    command_palette_input: Option<Entity<Buffer>>,
    command_palette_commands: Vec<CommandInfo>,
    command_palette_filtered: Vec<CommandInfo>,
    command_palette_selected: usize,
    command_palette_previous_mode: Option<String>,
    // Worktree (shared across cloned Stoats)
    worktree: Arc<Mutex<Worktree>>,
}

impl Stoat {
    pub fn new(cx: &mut App) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Create initial buffer item
        let item = cx.new(|cx| BufferItem::new(buffer, Language::PlainText, cx));

        // Create items vec with initial item (using enum_dispatch)
        let items = vec![ItemVariant::Buffer(item)];

        // Initialize worktree for instant file finder
        let worktree = Arc::new(Mutex::new(Worktree::new(PathBuf::from("."))));

        Self {
            items,
            active_item_index: 0,
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
            file_finder_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            command_palette_input: None,
            command_palette_commands: Vec::new(),
            command_palette_filtered: Vec::new(),
            command_palette_selected: 0,
            command_palette_previous_mode: None,
            worktree,
        }
    }

    /// Add a new item to the editor and activate it.
    ///
    /// The item is added at the end of the items list and becomes the active item.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to add (ItemVariant enum supporting any item type)
    pub fn add_item(&mut self, item: ItemVariant) {
        self.items.push(item);
        self.active_item_index = self.items.len() - 1;
    }

    /// Activate an item by index.
    ///
    /// If the index is out of bounds, does nothing.
    ///
    /// # Arguments
    ///
    /// * `index` - Index of the item to activate
    pub fn activate_item(&mut self, index: usize) {
        if index < self.items.len() {
            self.active_item_index = index;
        }
    }

    /// Get the active buffer item.
    ///
    /// Used by GUI layer to access buffer item properties like diff state.
    ///
    /// # Panics
    ///
    /// Panics if there are no items or if the active item is not a BufferItem.
    pub fn active_buffer_item(&self, _cx: &App) -> Entity<BufferItem> {
        self.items[self.active_item_index]
            .as_buffer()
            .expect("Active item must be a BufferItem")
            .clone()
    }

    pub fn buffer(&self, cx: &App) -> Entity<Buffer> {
        self.active_buffer_item(cx).read(cx).buffer().clone()
    }

    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.active_buffer_item(cx).read(cx).buffer_snapshot(cx)
    }

    pub fn token_snapshot(&self, cx: &App) -> TokenSnapshot {
        self.active_buffer_item(cx).read(cx).token_snapshot()
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

                // Update active buffer item with new content and language
                let active_item = self.active_buffer_item(cx);
                active_item.update(cx, |item, cx| {
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

                    // Compute git diff if file is in a repository
                    if let Ok(repo) = crate::git_repository::Repository::discover(first_path) {
                        if let Ok(head_content) = repo.head_content(first_path) {
                            let buffer_snapshot = item.buffer().read(cx).snapshot();
                            let buffer_id = buffer_snapshot.remote_id();

                            match crate::git_diff::BufferDiff::new(
                                buffer_id,
                                head_content,
                                &buffer_snapshot,
                            ) {
                                Ok(diff) => {
                                    tracing::debug!(
                                        "Computed diff with {} hunks",
                                        diff.hunks.len()
                                    );
                                    item.set_diff(Some(diff));
                                },
                                Err(e) => {
                                    tracing::warn!("Failed to compute diff: {}", e);
                                },
                            }
                        }
                        // File not in git HEAD (new/untracked) - this is expected
                    }
                    // Not in git repository - this is expected for non-git files
                });
            }
        }
    }

    pub fn buffer_contents(&self, cx: &App) -> String {
        self.active_buffer_item(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .text()
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

    /// Get the command palette query text from the input buffer
    pub fn command_palette_query(&self, cx: &App) -> String {
        self.command_palette_input
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

    /// Get the file preview data for the selected file (text + syntax tokens)
    pub fn file_finder_preview_data(&self) -> Option<&PreviewData> {
        self.file_finder_preview.as_ref()
    }

    /// Load preview for the currently selected file in the file finder.
    ///
    /// Gets the path of the currently selected file and loads its preview.
    /// Updates [`file_finder_preview`] with parsed syntax-highlighted content.
    /// Does nothing if no file is selected or if file finder is not in use.
    fn load_preview_for_selected(&mut self) {
        if let Some(path) = self.file_finder_filtered.get(self.file_finder_selected) {
            // Build absolute path from worktree root
            let root = self.worktree.lock().snapshot().root().to_path_buf();
            let abs_path = root.join(path);

            self.file_finder_preview = load_file_preview(&abs_path);
        } else {
            self.file_finder_preview = None;
        }
    }

    /// Filter files based on the current query.
    ///
    /// Performs true fuzzy matching on file paths using nucleo-matcher.
    /// Supports non-contiguous character matching (e.g., "stoedit" matches
    /// "stoat/src/actions/edit.rs"). Results are ranked by match quality score. Updates the
    /// filtered files list and resets selection to 0.
    pub fn filter_files(&mut self, query: &str) {
        if query.is_empty() {
            // No query: show all files
            self.file_finder_filtered = self
                .file_finder_files
                .iter()
                .map(|e| PathBuf::from(e.path.as_unix_str()))
                .collect();
        } else {
            // Parse pattern for smart fuzzy matching with case-insensitive and normalized matching
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            // Build candidate list with paths as strings
            let candidates: Vec<&str> = self
                .file_finder_files
                .iter()
                .map(|e| e.path.as_unix_str())
                .collect();

            // Match and score all candidates using nucleo-matcher
            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);

            // Sort by score (descending - higher score = better match)
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            // Limit to top 100 results
            matches.truncate(100);

            // Convert to PathBuf for display
            self.file_finder_filtered = matches
                .into_iter()
                .map(|(path, _score)| PathBuf::from(path))
                .collect();
        }

        // Reset selection to top when filter changes
        self.file_finder_selected = 0;

        // Load preview for the newly selected file (top of filtered list)
        self.load_preview_for_selected();
    }

    /// Filter commands based on fuzzy search query.
    ///
    /// Performs fuzzy matching on command names and descriptions using nucleo-matcher.
    /// Supports non-contiguous character matching. Results are ranked by match quality score.
    /// Updates the filtered commands list and resets selection to 0.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query string
    pub fn filter_commands(&mut self, query: &str) {
        if query.is_empty() {
            // No query: show all commands
            self.command_palette_filtered = self.command_palette_commands.clone();
        } else {
            // Parse pattern for smart fuzzy matching
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            // Build candidate list - search in both command name and description
            let candidates: Vec<String> = self
                .command_palette_commands
                .iter()
                .map(|cmd| format!("{} {}", cmd.name, cmd.description))
                .collect();

            let candidate_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();

            // Create a temporary matcher for commands (uses default config, not path-specific)
            let mut matcher = Matcher::new(Config::DEFAULT);

            // Match and score all candidates
            let mut matches = pattern.match_list(&candidate_refs, &mut matcher);

            // Sort by score (descending - higher score = better match)
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            // Limit to top 50 results
            matches.truncate(50);

            // Convert back to CommandInfo
            self.command_palette_filtered = matches
                .into_iter()
                .filter_map(|(matched_str, _score)| {
                    // Find the original CommandInfo by matching the search string
                    self.command_palette_commands.iter().find(|cmd| {
                        let search_str = format!("{} {}", cmd.name, cmd.description);
                        search_str == *matched_str
                    })
                })
                .cloned()
                .collect();
        }

        // Reset selection to top when filter changes
        self.command_palette_selected = 0;
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

impl Clone for Stoat {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            active_item_index: self.active_item_index,
            scroll: self.scroll.clone(),
            cursor_manager: self.cursor_manager.clone(),
            viewport_lines: self.viewport_lines,
            modes: self.modes.clone(),
            current_mode: self.current_mode.clone(),
            file_finder_input: self.file_finder_input.clone(),
            file_finder_files: self.file_finder_files.clone(),
            file_finder_filtered: self.file_finder_filtered.clone(),
            file_finder_selected: self.file_finder_selected,
            file_finder_previous_mode: self.file_finder_previous_mode.clone(),
            file_finder_preview: self.file_finder_preview.clone(),
            file_finder_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            command_palette_input: self.command_palette_input.clone(),
            command_palette_commands: self.command_palette_commands.clone(),
            command_palette_filtered: self.command_palette_filtered.clone(),
            command_palette_selected: self.command_palette_selected,
            command_palette_previous_mode: self.command_palette_previous_mode.clone(),
            worktree: self.worktree.clone(),
        }
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

#[cfg(test)]
mod fuzzy_matching_tests {
    use crate::Stoat;
    use std::path::PathBuf;

    #[test]
    fn matches_non_contiguous_chars() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Query "stoedit" should match "stoat/src/actions/edit.rs"
        // s -> stoat
        // t -> (already matched)
        // o -> (already matched)
        // e -> actions/edit
        // d -> edit
        // i -> edit
        // t -> edit
        s.filter_files("stoedit");

        let filtered = s.file_finder_filtered();
        assert!(
            filtered
                .iter()
                .any(|p| p.to_string_lossy().contains("actions/edit.rs")),
            "Expected 'stoedit' to match 'stoat/src/actions/edit.rs', but it didn't. Matches: {:?}",
            filtered
        );
    }

    #[test]
    fn case_insensitive_matching() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Uppercase query should match lowercase files
        s.filter_files("EDIT");

        let filtered = s.file_finder_filtered();
        assert!(
            filtered
                .iter()
                .any(|p| p.to_string_lossy().to_lowercase().contains("edit")),
            "Expected case-insensitive match for 'EDIT', but got: {:?}",
            filtered
        );
    }

    #[test]
    fn empty_query_shows_all_files() {
        let mut s = Stoat::test();
        s.open_file_finder();

        let all_files_count = s.file_finder_files().len();

        s.filter_files("");

        let filtered_count = s.file_finder_filtered().len();
        assert_eq!(
            all_files_count, filtered_count,
            "Empty query should show all files. Expected {}, got {}",
            all_files_count, filtered_count
        );
    }

    #[test]
    fn limits_results_to_100() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Use a very broad query that would match many files
        s.filter_files("s");

        let filtered = s.file_finder_filtered();
        assert!(
            filtered.len() <= 100,
            "Expected at most 100 results, got {}",
            filtered.len()
        );
    }

    #[test]
    fn resets_selection_on_filter() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Set selection to non-zero
        s.set_file_finder_selected(5);
        assert_eq!(s.file_finder_selected(), 5);

        // Filter should reset to 0
        s.filter_files("test");

        assert_eq!(
            s.file_finder_selected(),
            0,
            "Expected selection to reset to 0 after filtering"
        );
    }

    #[test]
    fn query_with_no_matches_returns_empty() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Query that should match no files
        s.filter_files("xyzqwertyuiopasdfghjkl");

        let filtered = s.file_finder_filtered();
        assert!(
            filtered.is_empty(),
            "Expected no matches for nonsense query, got: {:?}",
            filtered
        );
    }

    #[test]
    fn prefers_better_matches() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Query "lib" should rank lib.rs higher than other files containing "lib"
        s.filter_files("lib");

        let filtered = s.file_finder_filtered();
        if !filtered.is_empty() {
            // First result should be the best match
            let first = &filtered[0];
            let first_str = first.to_string_lossy();

            // lib.rs should be highly ranked
            assert!(
                first_str.contains("lib.rs")
                    || filtered
                        .iter()
                        .take(3)
                        .any(|p| p.to_string_lossy().contains("lib.rs")),
                "Expected 'lib.rs' to be in top 3 matches for query 'lib', but top result was: {}. Top 3: {:?}",
                first_str,
                filtered.iter().take(3).collect::<Vec<_>>()
            );
        }
    }
}
