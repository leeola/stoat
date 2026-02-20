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
pub mod git_fixture;

use crate::{
    buffer::item::BufferItem,
    keymap::{
        compiled::{CompiledKey, CompiledKeymap},
        dispatch::dispatch_editor_action,
    },
    stoat::KeyContext,
    Stoat,
};
use gpui::{App, AppContext, Context, Entity, TestAppContext};
use std::{path::PathBuf, sync::Arc};
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
            let compiled_keymap = test_keymap();
            let stoat = Stoat::new_with_text(
                crate::config::Config::default(),
                worktree,
                buffer_store,
                None,
                compiled_keymap,
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
            .read_entity(&self.entity, |s, _| s.review_state.files.clone())
    }

    /// Get current file/hunk position in diff review.
    ///
    /// Returns `(file_idx, hunk_idx)` tuple representing current position.
    pub fn diff_review_position(&self) -> (usize, usize) {
        self.cx.read_entity(&self.entity, |s, _| {
            (s.review_state.file_idx, s.review_state.hunk_idx)
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

    /// Get conflict review file list.
    pub fn conflict_files(&self) -> Vec<PathBuf> {
        self.cx
            .read_entity(&self.entity, |s, _| s.conflict_state.files.clone())
    }

    /// Get current file/conflict position in conflict review.
    ///
    /// Returns `(file_idx, conflict_idx)` tuple.
    pub fn conflict_position(&self) -> (usize, usize) {
        self.cx.read_entity(&self.entity, |s, _| {
            (s.conflict_state.file_idx, s.conflict_state.conflict_idx)
        })
    }

    /// Get conflict count for active buffer.
    pub fn conflict_count(&self) -> usize {
        self.read_buffer(|item, _| item.conflicts().len())
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

    /// Set the worktree to a [`GitFixture`]'s directory.
    ///
    /// Required for operations that resolve paths against the worktree root
    /// (e.g. stage/unstage). Chainable like [`init_git`](Self::init_git).
    pub fn use_fixture(mut self, fixture: &git_fixture::GitFixture) -> Self {
        self.update(|s, _cx| {
            s.worktree = Arc::new(parking_lot::Mutex::new(crate::worktree::Worktree::new(
                fixture.dir().to_path_buf(),
            )));
        });
        self
    }

    /// Read from the active [`BufferItem`] without needing nested entity access.
    pub fn read_buffer<R>(&self, f: impl FnOnce(&BufferItem, &App) -> R) -> R {
        self.cx.read_entity(&self.entity, |s, cx| {
            let item = s.active_buffer(cx);
            cx.read_entity(&item, |item, cx| f(item, cx))
        })
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
            let buffer_item = s.active_buffer(cx);
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

            if let Some(sel) = parsed.selections.first() {
                let start = offset_to_point(&parsed.text, sel.range.start);
                let end = offset_to_point(&parsed.text, sel.range.end);

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

                let cursor_pos = if sel.cursor_at_start { start } else { end };
                s.cursor.move_to(cursor_pos);
            } else if let Some(&offset) = parsed.cursors.first() {
                let point = offset_to_point(&parsed.text, offset);
                s.set_cursor_position(point);

                let id = s.selections.next_id();
                s.selections.select(
                    vec![text::Selection {
                        id,
                        start: point,
                        end: point,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );
            }
        });

        Ok(test_stoat)
    }

    /// Convert current buffer state to cursor notation string.
    ///
    /// Supports multiple selections/cursors. When all selections are empty
    /// (cursors only), outputs cursor markers. Otherwise outputs selection
    /// range markers for all non-empty selections.
    pub fn to_cursor_notation(&self) -> String {
        let text = self.buffer_text();
        self.cx.read_entity(&self.entity, |s, cx| {
            let selections = s.active_selections(cx);

            if selections.iter().all(|sel| sel.is_empty()) {
                let offsets: Vec<usize> = selections
                    .iter()
                    .map(|sel| point_to_offset(&text, sel.head()))
                    .collect();
                cursor_notation::format(&text, &offsets, &[])
            } else {
                let notation_sels: Vec<cursor_notation::Selection> = selections
                    .iter()
                    .filter(|sel| !sel.is_empty())
                    .map(|sel| cursor_notation::Selection {
                        range: point_to_offset(&text, sel.start)..point_to_offset(&text, sel.end),
                        cursor_at_start: sel.reversed,
                    })
                    .collect();
                cursor_notation::format(&text, &[], &notation_sels)
            }
        })
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

    /// Reverse-lookup an action name in the compiled keymap, then feed the
    /// resulting keystroke through the full [`type_key`](Self::type_key) pipeline.
    ///
    /// Use this instead of `type_key("x")` when the key is a keymap-bound action
    /// so that tests don't break if the keymap changes.
    pub fn type_action(&mut self, action: &str) {
        let keystroke = self.cx.read_entity(&self.entity, |s, _| {
            s.compiled_keymap
                .reverse_lookup(action, s)
                .unwrap_or_else(|| {
                    panic!(
                        "no binding for {:?} in mode={:?} focus={:?}",
                        action,
                        s.mode(),
                        s.key_context().as_str(),
                    )
                })
                .key
                .to_keystroke()
        });
        self.type_key_raw(keystroke);
    }

    /// Simulate a keystroke through the same interceptor pipeline as the GUI.
    ///
    /// Processes the key through select-regex, replace-char, find-char interceptors,
    /// digit accumulation, keymap lookup, and insert-text fallback -- mirroring
    /// [`EditorView::handle_key_down`](crate::editor::view::EditorView).
    pub fn type_key(&mut self, key: &str) {
        self.type_key_raw(make_keystroke(key));
    }

    fn type_key_raw(&mut self, keystroke: gpui::Keystroke) {
        let no_modifiers = !keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.shift
            && !keystroke.modifiers.platform;

        // Select-regex interceptor
        if self
            .cx
            .read_entity(&self.entity, |s, _| s.select_regex_pending.is_some())
        {
            match keystroke.key.as_ref() {
                "enter" => {
                    self.update(|s, cx| s.select_regex_submit(cx));
                    return;
                },
                "backspace" => {
                    self.update(|s, cx| {
                        if let Some(p) = &mut s.select_regex_pending {
                            p.pop();
                        }
                        s.select_regex_preview(cx);
                    });
                    return;
                },
                "escape" => {
                    self.update(|s, cx| s.select_regex_cancel(cx));
                    return;
                },
                _ => {
                    if let Some(key_char) = &keystroke.key_char {
                        let kc = key_char.clone();
                        self.update(|s, cx| {
                            if let Some(p) = &mut s.select_regex_pending {
                                p.push_str(&kc);
                            }
                            s.select_regex_preview(cx);
                        });
                    }
                    return;
                },
            }
        }

        // Replace-char interceptor
        if self.cx.read_entity(&self.entity, |s, _| s.replace_pending) {
            if let Some(key_char) = &keystroke.key_char {
                if key_char == "\u{1b}" {
                    self.update(|s, _| s.replace_pending = false);
                } else {
                    let kc = key_char.clone();
                    self.update(|s, cx| {
                        s.replace_pending = false;
                        s.replace_char_with(&kc, cx);
                    });
                }
            } else {
                self.update(|s, _| s.replace_pending = false);
            }
            return;
        }

        // Find-char interceptor
        if let Some(find_mode) = self
            .cx
            .read_entity(&self.entity, |s, _| s.find_char_pending)
        {
            if let Some(key_char) = &keystroke.key_char {
                if key_char == "\u{1b}" {
                    self.update(|s, _| s.find_char_pending = None);
                } else {
                    let kc = key_char.clone();
                    self.update(|s, cx| {
                        s.find_char_pending = None;
                        s.find_char_with(&kc, find_mode, cx);
                    });
                }
            } else {
                self.update(|s, _| s.find_char_pending = None);
            }
            return;
        }

        // Digit accumulation for count prefix
        if no_modifiers {
            let (key_context, mode) = self
                .cx
                .read_entity(&self.entity, |s, _| (s.key_context(), s.mode().to_string()));
            if key_context == KeyContext::TextEditor && (mode == "normal" || mode == "visual") {
                if let Some(key_char) = &keystroke.key_char {
                    if let Some(digit) = key_char.chars().next().and_then(|c| c.to_digit(10)) {
                        let has_pending = self
                            .cx
                            .read_entity(&self.entity, |s, _| s.pending_count.is_some());
                        if digit >= 1 || has_pending {
                            self.update(|s, _| {
                                let current = s.pending_count.unwrap_or(0);
                                s.pending_count =
                                    Some(current.saturating_mul(10).saturating_add(digit));
                            });
                            return;
                        }
                    }
                }
            }
        }

        // Keymap lookup
        let compiled_key = CompiledKey::from_keystroke(&keystroke);
        let matched_action = self.cx.read_entity(&self.entity, |s, _| {
            s.compiled_keymap
                .lookup(&compiled_key, s)
                .map(|b| b.action.clone())
        });

        if let Some(action) = matched_action {
            let mode_before = self
                .cx
                .read_entity(&self.entity, |s, _| s.mode().to_string());
            if dispatch_editor_action(&self.entity, &action, self.cx) {
                let is_transient = mode_before == "goto" || mode_before == "buffer";
                if is_transient
                    && self
                        .cx
                        .read_entity(&self.entity, |s, _| s.mode() == mode_before)
                {
                    self.update(|s, cx| s.set_mode_by_name("normal", cx));
                }
                self.update(|s, _| s.pending_count = None);
                return;
            }
            self.update(|s, _| s.pending_count = None);
            return;
        }

        // InsertText fallback
        let (key_context, mode) = self
            .cx
            .read_entity(&self.entity, |s, _| (s.key_context(), s.mode().to_string()));
        let should_insert = match key_context {
            KeyContext::FileFinder | KeyContext::CommandPalette | KeyContext::BufferFinder => true,
            KeyContext::TextEditor => mode == "insert",
            _ => false,
        };
        if should_insert {
            if let Some(key_char) = &keystroke.key_char {
                let kc = key_char.clone();
                self.update(|s, cx| s.insert_text(&kc, cx));
            }
        }
        self.update(|s, _| s.pending_count = None);
    }
}

fn make_keystroke(key: &str) -> gpui::Keystroke {
    match key {
        "enter" | "escape" | "backspace" | "tab" | "space" => gpui::Keystroke {
            key: key.into(),
            modifiers: gpui::Modifiers::default(),
            key_char: match key {
                "enter" => Some("\n".into()),
                "space" => Some(" ".into()),
                "tab" => Some("\t".into()),
                _ => None,
            },
        },
        _ if key.len() == 1 => gpui::Keystroke {
            key: key.into(),
            modifiers: gpui::Modifiers::default(),
            key_char: Some(key.into()),
        },
        _ => gpui::Keystroke {
            key: key.into(),
            modifiers: gpui::Modifiers::default(),
            key_char: None,
        },
    }
}

fn test_keymap() -> Arc<CompiledKeymap> {
    let source = include_str!("../../keymap.stcfg");
    let (config, _errors) = stoat_config::parse(source);
    Arc::new(
        config
            .map(|c| CompiledKeymap::compile(&c))
            .unwrap_or_else(|| CompiledKeymap { bindings: vec![] }),
    )
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
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
        assert_eq!(stoat.to_cursor_notation(), "hello |world");
    }

    #[gpui::test]
    fn to_cursor_notation_multiline(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("line1\nli|ne2\nline3", cx).unwrap();
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

    #[gpui::test]
    fn fixture_load_file_read_buffer(cx: &mut TestAppContext) {
        let fixture = git_fixture::GitFixture::load("basic-diff");
        let mut stoat = Stoat::test(cx).use_fixture(&fixture);
        stoat.update(|s, cx| {
            s.load_file(&fixture.changed_files()[0], cx).unwrap();
        });

        let hunk_count = stoat.read_buffer(|item, _cx| item.diff().map(|d| d.hunks.len()));
        assert!(hunk_count.is_some_and(|n| n > 0));
    }
}
