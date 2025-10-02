//! Symbol-based selection operations
//!
//! This module provides commands for selecting symbols - identifiers, keywords, and literals -
//! while skipping punctuation and operators. This enables semantic navigation through code,
//! jumping between meaningful named entities rather than every syntactic token.
//!
//! # Symbol Selection
//!
//! The primary commands are:
//! - [`select_next_symbol`] - finds the next symbol from the cursor position
//! - [`select_prev_symbol`] - finds the previous symbol from the cursor position
//!
//! Both automatically skip:
//! - Whitespace and newlines
//! - Punctuation (`.`, `,`, `;`, etc.)
//! - Operators (`+`, `-`, `->`, etc.)
//! - Brackets and delimiters (`()`, `<>`, `{}`, etc.)
//!
//! This differs from token-based selection (see [`crate::selection`]) which selects any
//! syntactic token including punctuation.
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state where selection is applied
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::editor_selection`] - the action namespace for selection commands

mod select_next_symbol;
mod select_prev_symbol;
