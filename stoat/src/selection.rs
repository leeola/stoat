//! Token-based selection operations
//!
//! This module provides low-level commands for selecting text based on token boundaries,
//! selecting ANY syntactic token including punctuation, operators, and delimiters.
//!
//! For higher-level symbol selection that skips punctuation to select identifiers,
//! keywords, and literals, see [`crate::actions::selection`].

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the next token from the current cursor position.
    ///
    /// This command skips whitespace and selects the next syntactic token.
    /// Similar to Helix's word selection but token-aware. The selection is
    /// created in visual mode, anchored at the current cursor position.
    ///
    /// # Behavior
    /// - Skips whitespace and newlines to find the next token
    /// - Selects the entire token (respects token boundaries)
    /// - If cursor is mid-token, behavior TBD
    /// - If no next token exists, returns None
    ///
    /// # Returns
    /// The byte range of the selected token, or None if no token found.
    pub fn select_next_token(&self, cx: &App) -> Option<Range<usize>> {
        // TODO: Implement
        let _ = cx;
        None
    }
}

#[cfg(test)]
mod tests {
    // Note: Token-level selection tests have been removed since the `w` key
    // now performs symbol-based selection (see actions::selection module).
    // If token-level selection is needed in the future, it should be bound
    // to a different key.
}
