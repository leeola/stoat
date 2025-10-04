//! Shell-level actions
//!
//! Actions that operate at the shell level, including pane management and file finding.
//! Pane management actions are implemented in the GUI layer since they require access to
//! the pane tree structure.

mod file_finder_navigation;
mod open_file_finder;
