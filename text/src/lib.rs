//! Text editing crate for Stoat
//!
//! This crate provides the text editing functionality for the Stoat editor, built on top
//! of the rope AST data structure. It manages buffers, views, cursors, and language parsing
//! to enable efficient text editing within the node-based canvas environment.
//!
//! The key components are:
//! - [`buffer::Buffer`] - Manages text content using rope AST
//! - [`cursor::Cursor`] - Tracks cursor positions and selections
//! - [`view::View`] - Defines viewports into buffers
//! - [`node::Node`] - Integrates text editing into the Stoat node system
//! - [`parse::Parse`] - Handles language-specific parsing into rope AST

pub mod buffer;
pub mod cursor;
pub mod node;
pub mod parse;
pub mod view;
