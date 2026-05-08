//! TUI-free diff API. Re-exports the structural-diff hunk extractor
//! and supporting row/side/hunk types from the in-tree review module
//! so downstream consumers -- the bin layer's `stoat diff` subcommand
//! and the diff-cache RPC -- can compute and consume the same
//! per-file hunks the review pane renders without depending on the
//! TUI rendering path.

pub use crate::review::{
    extract_review_hunks_changeset, MoveProvenance, ReviewFileInput, ReviewHunk, ReviewRow,
    ReviewSide,
};
