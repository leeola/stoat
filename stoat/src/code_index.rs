//! On-disk persistence for the code-interaction index.
//!
//! Wraps the [`codegraph`] crate's pure model with the stoat-side IO: where
//! an index lives on disk and how its manifest and per-file shards are read
//! and written. The scheduling that builds and refreshes the index lands in
//! sibling modules.

pub(crate) mod store;
