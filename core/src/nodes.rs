//! Node implementations for the Stoat editor
//!
//! This module contains all concrete node implementations that can be used
//! in the workspace. Each node type implements the `Node` trait defined in
//! the parent `node` module.

#[cfg(feature = "csv")]
pub mod csv;

#[cfg(feature = "json")]
pub mod json;

pub mod map;
pub mod table;

#[cfg(test)]
mod integration_tests;

// Re-export node implementations for convenience
#[cfg(feature = "csv")]
pub use csv::CsvSourceNode;
#[cfg(feature = "json")]
pub use json::JsonSourceNode;
pub use map::MapNode;
pub use table::TableViewerNode;
