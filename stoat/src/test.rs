//! Test utilities for Stoat v4.
//!
//! This module provides GPUI-native test infrastructure for validating the Entity pattern
//! and enabling test-driven development of editor features.
//!
//! # Key Components
//!
//! - [`cursor_notation`] - DSL for cursor/selection positions in test strings
//! - [`TestStoat`] - Wrapper around [`Entity<Stoat>`] with test-oriented helpers
//!
//! # Example
//!
//! ```ignore
//! #[gpui::test]
//! fn test_insert_mode(cx: &mut TestAppContext) {
//!     let stoat = Stoat::test(cx);
//!
//!     stoat.update(cx, |s, cx| {
//!         s.enter_insert_mode(cx);
//!         s.insert_text("hello", cx);
//!     });
//!
//!     assert_eq!(stoat.buffer_text(cx), "hello");
//! }
//! ```

pub mod cursor_notation;

use crate::{actions::*, Stoat};
use gpui::{Action, AppContext, Context, Entity, TestAppContext};
use std::{any::TypeId, path::PathBuf, sync::Arc};
use text::Point;

/// Wrapper around [`Entity<Stoat>`] that provides test-oriented helper methods.
///
/// This wrapper makes tests cleaner by providing convenient accessors for common
/// operations like reading buffer text, cursor position, and mode. It holds both
/// the entity and the test context, so you don't need to pass `cx` to every method.
///
/// # Creation
///
/// Use [`Stoat::test`] or [`Stoat::test_with_text`] to create instances:
///
/// ```ignore
/// let mut stoat = Stoat::test(cx);  // cx is now owned by stoat
/// let mut stoat = Stoat::test_with_text("hello", cx);
/// ```
///
/// Note: Once created, `cx` is borrowed by the `TestStoat` for its lifetime.
///
/// # Usage
///
/// The wrapper provides both read and update operations without needing `cx`:
///
/// ```ignore
/// // Read operations - no cx needed!
/// let text = stoat.buffer_text();
/// let pos = stoat.cursor_position();
/// let mode = stoat.mode();
///
/// // Update operations - no outer cx needed!
/// stoat.update(|s, cx| {
///     s.insert_text("hello", cx);
/// });
/// ```
pub struct TestStoat<'a> {
    entity: Entity<Stoat>,
    cx: &'a mut TestAppContext,
    temp_dir: Option<tempfile::TempDir>,
    repo_path: Option<PathBuf>,
}

impl<'a> TestStoat<'a> {
    /// Create a new TestStoat with the given initial text.
    ///
    /// Called by [`Stoat::test`] and [`Stoat::test_with_text`].
    pub fn new(text: &str, cx: &'a mut TestAppContext) -> Self {
        let entity = cx.new(|cx| {
            // Create test-specific worktree and buffer_store
            let worktree = Arc::new(parking_lot::Mutex::new(crate::worktree::Worktree::new(
                std::path::PathBuf::from("."),
            )));
            let buffer_store = cx.new(|_| crate::buffer::store::BufferStore::new());

            // Create Stoat with test text directly (avoids buffer edit after creation)
            let empty_keymap =
                Arc::new(crate::keymap::compiled::CompiledKeymap { bindings: vec![] });
            let mut stoat = Stoat::new_with_text(
                crate::config::Config::default(),
                worktree,
                buffer_store,
                None,
                empty_keymap,
                text,
                cx,
            );

            // Set language to Rust for better tokenization in tests
            stoat.active_buffer(cx).update(cx, |item, cx| {
                item.set_language(stoat_text::Language::Rust);
                let _ = item.reparse(cx);
            });

            stoat
        });

        Self {
            entity,
            cx,
            temp_dir: None,
            repo_path: None,
        }
    }

    /// Get access to the underlying [`Entity<Stoat>`].
    ///
    /// Use this when you need to interact with APIs that expect an entity directly.
    pub fn entity(&self) -> &Entity<Stoat> {
        &self.entity
    }

    /// Update the Stoat entity.
    ///
    /// No need to pass `cx` - it's stored in the wrapper!
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Stoat, &mut Context<Stoat>) -> R) -> R {
        self.entity.update(self.cx, f)
    }

    /// Get the current buffer text.
    ///
    /// No need to pass `cx` - it's stored in the wrapper!
    pub fn buffer_text(&self) -> String {
        self.cx.read_entity(&self.entity, |s, cx| {
            cx.read_entity(&s.active_buffer(cx), |item, cx| {
                cx.read_entity(item.buffer(), |buffer, _| buffer.text())
            })
        })
    }

    /// Get the current cursor position.
    ///
    /// Returns the cursor as a [`text::Point`] with row and column.
    pub fn cursor_position(&self) -> Point {
        self.cx
            .read_entity(&self.entity, |s, _| s.cursor_position())
    }

    /// Get the current mode.
    pub fn mode(&self) -> String {
        self.cx
            .read_entity(&self.entity, |s, _| s.mode().to_string())
    }

    /// Get the current selection.
    ///
    /// Returns a copy of the current selection including start, end, and reversed flag.
    /// Now reads from the multi-cursor API for compatibility with migrated actions.
    pub fn selection(&self) -> crate::cursor::Selection {
        self.cx.read_entity(&self.entity, |s, cx| {
            // Get newest selection from multi-cursor API
            let buffer_item = s.active_buffer(cx);
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let sel = s.selections.newest::<text::Point>(&buffer_snapshot);

            // Convert to legacy cursor::Selection format
            crate::cursor::Selection {
                start: sel.start,
                end: sel.end,
                reversed: sel.reversed,
            }
        })
    }

    /// Get diff review file list.
    ///
    /// Returns the list of files being reviewed in diff review mode.
    /// Empty if not in diff review mode.
    pub fn diff_review_files(&self) -> Vec<PathBuf> {
        self.cx
            .read_entity(&self.entity, |s, _| s.diff_review_files.clone())
    }

    /// Get current file/hunk position in diff review.
    ///
    /// Returns `(file_idx, hunk_idx)` tuple representing current position.
    pub fn diff_review_position(&self) -> (usize, usize) {
        self.cx.read_entity(&self.entity, |s, _| {
            (
                s.diff_review_current_file_idx,
                s.diff_review_current_hunk_idx,
            )
        })
    }

    /// Get hunk count for active buffer.
    ///
    /// Returns the number of hunks in the currently active buffer's diff,
    /// or `None` if no diff is loaded.
    pub fn hunk_count(&self) -> Option<usize> {
        self.cx.read_entity(&self.entity, |s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.read(cx).diff().map(|d| d.hunks.len())
        })
    }

    /// Get the test repository path.
    ///
    /// Returns the path to the temporary git repository created by [`init_git`](Self::init_git),
    /// or [`None`] if no git repository has been initialized.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stoat = Stoat::test(cx).init_git();
    /// let repo_path = stoat.repo_path().unwrap();
    /// let file_path = repo_path.join("test.txt");
    /// ```
    pub fn repo_path(&self) -> Option<&std::path::Path> {
        self.repo_path.as_deref()
    }

    /// Set the current file path on the Stoat instance.
    ///
    /// Updates the `current_file_path` field on the underlying [`Stoat`] entity.
    /// This is useful in tests to associate a buffer with a file path before
    /// calling file operations like [`write_file`](crate::Stoat::write_file).
    ///
    /// # Arguments
    ///
    /// * `path` - The file path to associate with the current buffer
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stoat = Stoat::test(cx).init_git();
    /// let file_path = stoat.repo_path().unwrap().join("test.txt");
    /// stoat.set_file_path(file_path.clone());
    /// stoat.update(|s, cx| {
    ///     s.write_file(cx).expect("write failed");
    /// });
    /// ```
    pub fn set_file_path(&mut self, path: PathBuf) {
        self.update(|s, _cx| {
            s.current_file_path = Some(path);
        });
    }

    /// Dispatch an action to the Stoat entity.
    ///
    /// This provides a type-safe way to test actions, routing them to the appropriate
    /// Stoat methods just like the GUI's action handlers do. This ensures tests exercise
    /// the same code paths as the real editor.
    ///
    /// # Arguments
    ///
    /// * `action` - The action to dispatch (e.g., [`WriteFile`], [`InsertText`], [`MoveLeft`])
    ///
    /// # Panics
    ///
    /// Panics if the action fails or if the action type is not supported. The panic
    /// location will point to the caller's dispatch site using `#[track_caller]`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stoat = Stoat::test(cx).init_git();
    /// let file_path = stoat.repo_path().unwrap().join("test.txt");
    /// stoat.set_file_path(file_path);
    ///
    /// // Dispatch actions - no error handling needed!
    /// stoat.dispatch(EnterInsertMode);
    /// stoat.dispatch(InsertText("Hello".to_string()));
    /// stoat.dispatch(WriteFile);
    /// ```
    #[track_caller]
    pub fn dispatch<A: Action>(&mut self, action: A) {
        let type_id = TypeId::of::<A>();

        // Match on action TypeId and call corresponding Stoat method
        // Movement actions
        if type_id == TypeId::of::<MoveLeft>() {
            self.update(|s, cx| s.move_left(cx));
        } else if type_id == TypeId::of::<MoveRight>() {
            self.update(|s, cx| s.move_right(cx));
        } else if type_id == TypeId::of::<MoveUp>() {
            self.update(|s, cx| s.move_up(cx));
        } else if type_id == TypeId::of::<MoveDown>() {
            self.update(|s, cx| s.move_down(cx));
        } else if type_id == TypeId::of::<MoveWordLeft>() {
            self.update(|s, cx| s.move_word_left(cx));
        } else if type_id == TypeId::of::<MoveWordRight>() {
            self.update(|s, cx| s.move_word_right(cx));
        } else if type_id == TypeId::of::<MoveToLineStart>() {
            self.update(|s, cx| s.move_to_line_start(cx));
        } else if type_id == TypeId::of::<MoveToLineEnd>() {
            self.update(|s, cx| s.move_to_line_end(cx));
        } else if type_id == TypeId::of::<MoveToFileStart>() {
            self.update(|s, cx| s.move_to_file_start(cx));
        } else if type_id == TypeId::of::<MoveToFileEnd>() {
            self.update(|s, cx| s.move_to_file_end(cx));
        } else if type_id == TypeId::of::<PageUp>() {
            self.update(|s, cx| s.page_up(cx));
        } else if type_id == TypeId::of::<PageDown>() {
            self.update(|s, cx| s.page_down(cx));
        }
        // Edit actions
        else if type_id == TypeId::of::<DeleteLeft>() {
            self.update(|s, cx| s.delete_left(cx));
        } else if type_id == TypeId::of::<DeleteRight>() {
            self.update(|s, cx| s.delete_right(cx));
        } else if type_id == TypeId::of::<DeleteWordLeft>() {
            self.update(|s, cx| s.delete_word_left(cx));
        } else if type_id == TypeId::of::<DeleteWordRight>() {
            self.update(|s, cx| s.delete_word_right(cx));
        } else if type_id == TypeId::of::<NewLine>() {
            self.update(|s, cx| s.new_line(cx));
        } else if type_id == TypeId::of::<DeleteLine>() {
            self.update(|s, cx| s.delete_line(cx));
        } else if type_id == TypeId::of::<DeleteToEndOfLine>() {
            self.update(|s, cx| s.delete_to_end_of_line(cx));
        }
        // Mode actions
        else if type_id == TypeId::of::<EnterInsertMode>() {
            self.update(|s, cx| s.enter_insert_mode(cx));
        } else if type_id == TypeId::of::<EnterNormalMode>() {
            self.update(|s, cx| s.enter_normal_mode(cx));
        } else if type_id == TypeId::of::<EnterVisualMode>() {
            self.update(|s, cx| s.enter_visual_mode(cx));
        }
        // Parameterized actions
        else if type_id == TypeId::of::<InsertText>() {
            let action = unsafe { &*(&action as *const A as *const InsertText) };
            self.update(|s, cx| s.insert_text(&action.0, cx));
        } else if type_id == TypeId::of::<SetMode>() {
            let action = unsafe { &*(&action as *const A as *const SetMode) };
            self.update(|s, cx| s.set_mode_by_name(&action.0, cx));
        }
        // File actions
        else if type_id == TypeId::of::<WriteFile>() {
            self.update(|s, cx| {
                s.write_file(cx)
                    .unwrap_or_else(|e| panic!("WriteFile action failed: {e}"))
            });
        }
        // Git actions
        else if type_id == TypeId::of::<GitStageFile>() {
            self.update(|s, cx| {
                s.git_stage_file(cx)
                    .unwrap_or_else(|e| panic!("GitStageFile action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitStageAll>() {
            self.update(|s, cx| {
                s.git_stage_all(cx)
                    .unwrap_or_else(|e| panic!("GitStageAll action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitUnstageFile>() {
            self.update(|s, cx| {
                s.git_unstage_file(cx)
                    .unwrap_or_else(|e| panic!("GitUnstageFile action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitUnstageAll>() {
            self.update(|s, cx| {
                s.git_unstage_all(cx)
                    .unwrap_or_else(|e| panic!("GitUnstageAll action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitStageHunk>() {
            self.update(|s, cx| {
                s.git_stage_hunk(cx)
                    .unwrap_or_else(|e| panic!("GitStageHunk action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitUnstageHunk>() {
            self.update(|s, cx| {
                s.git_unstage_hunk(cx)
                    .unwrap_or_else(|e| panic!("GitUnstageHunk action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitToggleStageHunk>() {
            self.update(|s, cx| {
                s.git_toggle_stage_hunk(cx)
                    .unwrap_or_else(|e| panic!("GitToggleStageHunk action failed: {e}"))
            });
        } else if type_id == TypeId::of::<GitToggleStageLine>() {
            self.update(|s, cx| {
                s.git_toggle_stage_line(cx)
                    .unwrap_or_else(|e| panic!("GitToggleStageLine action failed: {e}"))
            });
        } else {
            panic!("Unsupported action type: {}", std::any::type_name::<A>());
        }
    }

    /// Initialize a git repository for testing.
    ///
    /// Creates a temporary directory, initializes a git repository in it, and configures
    /// basic git settings (user.name and user.email). The temp directory is kept alive
    /// for the lifetime of this [`TestStoat`] instance.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `git` command is not found on PATH
    /// - `git init` fails
    /// - Git configuration commands fail
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    ///
    /// # Example
    ///
    /// ```ignore
    /// #[gpui::test]
    /// fn test_with_git(cx: &mut TestAppContext) {
    ///     let stoat = Stoat::test(cx).init_git();
    ///     // Test git operations...
    /// }
    /// ```
    pub fn init_git(mut self) -> Self {
        use std::process::Command;

        // Create temporary directory
        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let repo_path = temp_dir.path().to_path_buf();

        // Initialize git repository
        let output = Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git init - is git installed?");

        if !output.status.success() {
            panic!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Configure git user.name
        let output = Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git config user.name");

        if !output.status.success() {
            panic!(
                "git config user.name failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Configure git user.email
        let output = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git config user.email");

        if !output.status.success() {
            panic!(
                "git config user.email failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Store temp_dir and repo_path
        self.temp_dir = Some(temp_dir);
        self.repo_path = Some(repo_path.clone());

        // Update the Stoat's worktree to point to the temp repo
        self.update(|s, _cx| {
            s.worktree = Arc::new(parking_lot::Mutex::new(crate::worktree::Worktree::new(
                repo_path,
            )));
        });

        self
    }

    /// Create a TestStoat with cursor and selection from marked notation.
    ///
    /// Uses the cursor notation DSL to specify initial cursor/selection positions.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Cursor at position 6
    /// let stoat = Stoat::test_with_cursor_notation("hello |world", cx);
    ///
    /// // Selection with cursor at end
    /// let stoat = Stoat::test_with_cursor_notation("<|hello||>", cx);
    /// ```
    pub fn test_with_cursor_notation(
        marked_text: &str,
        cx: &'a mut TestAppContext,
    ) -> anyhow::Result<Self> {
        let parsed = cursor_notation::parse(marked_text)?;

        let mut test_stoat = Self::new(&parsed.text, cx);

        test_stoat.update(|s, cx| {
            // Set cursor position if we have one
            if let Some(&offset) = parsed.cursors.first() {
                let point = offset_to_point(&parsed.text, offset);
                s.set_cursor_position(point);
            }

            // Set selection if we have one (use multi-cursor API)
            if let Some(sel) = parsed.selections.first() {
                let start = offset_to_point(&parsed.text, sel.range.start);
                let end = offset_to_point(&parsed.text, sel.range.end);

                // Get buffer snapshot for anchor-based storage
                let buffer_item = s.active_buffer(cx);
                let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

                // Create selection using multi-cursor API
                let id = s.selections.next_id();
                s.selections.select(
                    vec![text::Selection {
                        id,
                        start,
                        end,
                        reversed: sel.cursor_at_start,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );

                // Sync cursor position for backward compat
                let cursor_pos = if sel.cursor_at_start { start } else { end };
                s.cursor.move_to(cursor_pos);
            }
        });

        Ok(test_stoat)
    }

    /// Convert current buffer state to cursor notation string.
    ///
    /// Returns the buffer text with cursor and selection markers.
    pub fn to_cursor_notation(&self) -> String {
        let text = self.buffer_text();
        let cursor_pos = self.cursor_position();
        let selection = self.selection();

        let cursor_offset = point_to_offset(&text, cursor_pos);

        if selection.is_empty() {
            // Just a cursor
            cursor_notation::format(&text, &[cursor_offset], &[])
        } else {
            // Selection
            let start_offset = point_to_offset(&text, selection.start);
            let end_offset = point_to_offset(&text, selection.end);

            let notation_sel = cursor_notation::Selection {
                range: start_offset..end_offset,
                cursor_at_start: selection.reversed,
            };

            cursor_notation::format(&text, &[], &[notation_sel])
        }
    }

    /// Assert that buffer state matches expected cursor notation.
    ///
    /// Compares the current buffer state (text, cursor, selection) against
    /// the expected marked string.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// stoat.assert_cursor_notation("hello |world");
    /// stoat.assert_cursor_notation("<|hello||>");
    /// ```
    pub fn assert_cursor_notation(&self, expected: &str) {
        let actual = self.to_cursor_notation();
        assert_eq!(
            actual, expected,
            "Buffer state doesn't match expected cursor notation"
        );
    }
}

/// Convert absolute byte offset to Point (row, column).
fn offset_to_point(text: &str, offset: usize) -> Point {
    let mut current_offset = 0;
    let mut row = 0;

    for line in text.lines() {
        let line_len = line.len();
        let line_end = current_offset + line_len;

        if offset <= line_end {
            // Offset is on this line
            let col = offset - current_offset;
            return Point::new(row, col as u32);
        }

        // Move past this line plus newline
        current_offset = line_end + 1; // +1 for \n
        row += 1;
    }

    // Offset is at or past the end
    Point::new(row, offset.saturating_sub(current_offset) as u32)
}

/// Convert Point (row, column) to absolute byte offset.
fn point_to_offset(text: &str, point: Point) -> usize {
    let mut offset = 0;

    for (row, line) in text.lines().enumerate() {
        if row == point.row as usize {
            return offset + point.column as usize;
        }

        offset += line.len() + 1; // +1 for \n
    }

    // Point is past the end
    offset + point.column as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stoat;

    #[gpui::test]
    fn creates_test_stoat(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        // Should start in normal mode
        assert_eq!(stoat.mode(), "normal");

        // Should have empty buffer (for testing)
        assert_eq!(stoat.buffer_text(), "");
    }

    #[gpui::test]
    fn creates_test_stoat_with_text(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_text("hello world", cx);

        assert_eq!(stoat.buffer_text(), "hello world");
    }

    #[gpui::test]
    fn helper_reads_buffer_text(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_text("test", cx);

        assert_eq!(stoat.buffer_text(), "test");
    }

    #[gpui::test]
    fn helper_reads_cursor_position(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        assert_eq!(stoat.cursor_position(), Point::new(0, 0));
    }

    #[gpui::test]
    fn helper_reads_mode(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        assert_eq!(stoat.mode(), "normal");
    }

    // ===== Cursor Notation Tests =====

    #[gpui::test]
    fn test_with_cursor_notation_cursor_only(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello world");
        assert_eq!(stoat.cursor_position(), Point::new(0, 6));
        assert!(stoat.selection().is_empty());
    }

    #[gpui::test]
    fn test_with_cursor_notation_multiline(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("line1\nli|ne2\nline3", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "line1\nline2\nline3");
        assert_eq!(stoat.cursor_position(), Point::new(1, 2));
    }

    #[gpui::test]
    fn test_with_cursor_notation_selection_cursor_at_end(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("<|hello||>", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello");
        let selection = stoat.selection();
        assert!(!selection.is_empty());
        assert_eq!(selection.start, Point::new(0, 0));
        assert_eq!(selection.end, Point::new(0, 5));
        assert!(!selection.reversed);
    }

    #[gpui::test]
    fn test_with_cursor_notation_selection_cursor_at_start(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("<||hello|>", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello");
        let selection = stoat.selection();
        assert!(!selection.is_empty());
        assert_eq!(selection.start, Point::new(0, 0));
        assert_eq!(selection.end, Point::new(0, 5));
        assert!(selection.reversed);
    }

    #[gpui::test]
    fn to_cursor_notation_cursor_only(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("hello world", cx);

        stoat.update(|s, _cx| {
            s.set_cursor_position(Point::new(0, 6));
        });

        assert_eq!(stoat.to_cursor_notation(), "hello |world");
    }

    #[gpui::test]
    fn to_cursor_notation_multiline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2\nline3", cx);

        stoat.update(|s, _cx| {
            s.set_cursor_position(Point::new(1, 2));
        });

        assert_eq!(stoat.to_cursor_notation(), "line1\nli|ne2\nline3");
    }

    #[gpui::test]
    fn to_cursor_notation_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("hello", cx);

        stoat.update(|s, cx| {
            // Create selection using multi-cursor API
            let buffer_item = s.active_buffer(cx);
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 5),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &buffer_snapshot,
            );
            s.cursor.move_to(Point::new(0, 5));
        });

        assert_eq!(stoat.to_cursor_notation(), "<|hello||>");
    }

    #[gpui::test]
    fn assert_cursor_notation_success(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
        stoat.assert_cursor_notation("hello |world");
    }

    #[gpui::test]
    #[should_panic(expected = "Buffer state doesn't match expected cursor notation")]
    fn assert_cursor_notation_failure(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
        stoat.assert_cursor_notation("hello| world");
    }

    #[gpui::test]
    fn round_trip_cursor_notation(cx: &mut TestAppContext) {
        let input = "hello |world\nfoo bar";
        let stoat = Stoat::test_with_cursor_notation(input, cx).unwrap();
        assert_eq!(stoat.to_cursor_notation(), input);
    }

    #[gpui::test]
    fn round_trip_cursor_notation_selection(cx: &mut TestAppContext) {
        let input = "<|hello||> world";
        let stoat = Stoat::test_with_cursor_notation(input, cx).unwrap();
        assert_eq!(stoat.to_cursor_notation(), input);
    }

    // ===== Offset/Point Conversion Tests =====

    #[test]
    fn offset_to_point_single_line() {
        assert_eq!(offset_to_point("hello", 0), Point::new(0, 0));
        assert_eq!(offset_to_point("hello", 3), Point::new(0, 3));
        assert_eq!(offset_to_point("hello", 5), Point::new(0, 5));
    }

    #[test]
    fn offset_to_point_multiline() {
        let text = "line1\nline2\nline3";
        assert_eq!(offset_to_point(text, 0), Point::new(0, 0));
        assert_eq!(offset_to_point(text, 6), Point::new(1, 0)); // Start of line2
        assert_eq!(offset_to_point(text, 8), Point::new(1, 2)); // Middle of line2
        assert_eq!(offset_to_point(text, 12), Point::new(2, 0)); // Start of line3
    }

    #[test]
    fn point_to_offset_single_line() {
        assert_eq!(point_to_offset("hello", Point::new(0, 0)), 0);
        assert_eq!(point_to_offset("hello", Point::new(0, 3)), 3);
        assert_eq!(point_to_offset("hello", Point::new(0, 5)), 5);
    }

    #[test]
    fn point_to_offset_multiline() {
        let text = "line1\nline2\nline3";
        assert_eq!(point_to_offset(text, Point::new(0, 0)), 0);
        assert_eq!(point_to_offset(text, Point::new(1, 0)), 6); // Start of line2
        assert_eq!(point_to_offset(text, Point::new(1, 2)), 8); // Middle of line2
        assert_eq!(point_to_offset(text, Point::new(2, 0)), 12); // Start of line3
    }

    #[test]
    fn offset_point_round_trip() {
        let text = "hello\nworld\ntest";
        let offsets = vec![0, 3, 6, 10, 12];

        for offset in offsets {
            let point = offset_to_point(text, offset);
            let back = point_to_offset(text, point);
            assert_eq!(offset, back, "Round trip failed for offset {offset}");
        }
    }
}
