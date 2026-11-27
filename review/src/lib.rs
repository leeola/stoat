//! The review model, the per-file hunks a diff review shows and the views
//! built on them.
//!
//! [`review`] extracts the hunks and rows of a changeset, [`review_session`]
//! holds a review's files and chunks, and [`review_apply`] writes a chunk back
//! as a patch. [`merge_view`] aligns a conflict's ancestor with its two sides
//! in three columns. [`diff`], [`diff_cache`], and [`diff_render_cli`]
//! compute, cache, and print the same hunks for the `stoat diff` subcommand.

pub mod diff;
pub mod diff_cache;
pub mod diff_render_cli;
pub mod merge_view;
pub mod review;
pub mod review_apply;
pub mod review_session;
