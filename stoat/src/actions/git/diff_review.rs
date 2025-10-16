//! Diff review actions.
//!
//! This module contains actions for the diff review modal, which provides a guided
//! workflow for reviewing all changes in a repository hunk-by-hunk.

mod approve_hunk;
mod cycle_comparison_mode;
mod dismiss;
mod hunk_position;
mod next_hunk;
mod next_unreviewed_hunk;
mod open;
mod prev_hunk;
mod reset_progress;
mod toggle_approval;
