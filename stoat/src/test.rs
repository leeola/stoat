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

use crate::Stoat;
use gpui::{AppContext, Context, Entity, TestAppContext};
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
}

impl<'a> TestStoat<'a> {
    /// Create a new TestStoat with the given initial text.
    ///
    /// Called by [`Stoat::test`] and [`Stoat::test_with_text`].
    pub fn new(text: &str, cx: &'a mut TestAppContext) -> Self {
        let entity = cx.new(|cx| {
            let stoat = Stoat::new(cx);

            // Always update the buffer to replace welcome text (even with empty string)
            // Use Rust language for better tokenization in tests
            stoat.buffer_item().update(cx, |item, cx| {
                item.set_language(stoat_text::Language::Rust);
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, text)]);
                });
                let _ = item.reparse(cx);
            });

            stoat
        });

        Self { entity, cx }
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
            cx.read_entity(s.buffer_item(), |item, cx| {
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
    pub fn selection(&self) -> crate::cursor::Selection {
        self.cx
            .read_entity(&self.entity, |s, _| s.selection().clone())
    }
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
}
