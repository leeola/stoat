//! Diff review actions.
//!
//! This module contains actions for the diff review modal, which provides a guided
//! workflow for reviewing all changes in a repository hunk-by-hunk.

mod approve_hunk;
mod cycle_comparison_mode;
mod dismiss;
mod enter_line_select;
mod hunk_position;
mod line_select_all;
mod line_select_cancel;
mod line_select_stage;
mod line_select_toggle;
mod line_select_unstage;
mod next_hunk;
mod next_unreviewed_hunk;
mod open;
mod prev_hunk;
mod reset_progress;
mod toggle_approval;
