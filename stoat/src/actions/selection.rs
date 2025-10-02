//! Selection operations
//!
//! This module provides commands for selecting text based on both semantic (symbol) and
//! syntactic (token) boundaries.
//!
//! # Symbol Selection
//!
//! Symbol selection (`w`/`b`) selects meaningful named entities while skipping punctuation:
//! - [`select_next_symbol`] - finds the next symbol from the cursor position
//! - [`select_prev_symbol`] - finds the previous symbol from the cursor position
//!
//! Automatically skips:
//! - Whitespace and newlines
//! - Punctuation (`.`, `,`, `;`, etc.)
//! - Operators (`+`, `-`, `->`, etc.)
//! - Brackets and delimiters (`()`, `<>`, `{}`, etc.)
//!
//! # Token Selection
//!
//! Token selection (`W`/`B`) selects ANY syntactic token including punctuation:
//! - [`select_next_token`] - finds the next token from the cursor position
//! - [`select_prev_token`] - finds the previous token from the cursor position
//!
//! Selects ALL tokens:
//! - Identifiers, keywords, literals (same as symbols)
//! - Punctuation (`.`, `,`, `;`, etc.)
//! - Operators (`+`, `-`, `->`, etc.)
//! - Brackets and delimiters (`()`, `<>`, `{}`, etc.)
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state where selection is applied
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::editor_selection`] - the action namespace for selection commands

mod select_next_symbol;
mod select_next_token;
mod select_prev_symbol;
mod select_prev_token;
