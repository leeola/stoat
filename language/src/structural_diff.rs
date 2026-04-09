//! Tree-aware structural diff over [`stoat_text::Rope`] inputs.
//!
//! This module is the foundation for a Difftastic-style diff that operates
//! on tree-sitter parse trees. The full algorithm (Dijkstra over an edit
//! graph of `(lhs_pos, rhs_pos, parents_stack)` vertices, with content-id
//! preprocessing and slider correction) is a multi-PR project; the present
//! scaffolding establishes the public surface and the line-diff fallback
//! that the structural path will defer to on input that exceeds the graph
//! cap.
//!
//! Pipeline (target):
//!
//! ```text
//!   parse (tree-sitter)
//!     -> Syntax<List | Atom> arena    (mod arena, mod content_id)
//!         -> unchanged preprocessing  (mod unchanged)
//!         -> Dijkstra over edit graph (mod graph, mod dijkstra)
//!              -> slider correction   (mod sliders)
//!                  -> DiffChange list (mod output)
//!                       -> DiffMap population
//! ```
//!
//! Pipeline (current): line diff via Myers, returning [`DiffChange`]s at
//! line granularity. The structural path will replace
//! [`diff_lines`]'s implementation, not its signature.
//!
//! Reference: `references/difftastic/src/diff/`. Algorithm and cost values
//! are taken from there; see in-code citations on each ported piece.

mod arena;
mod content_id;
mod dijkstra;
mod graph;
mod line_diff;
mod lower;
mod sliders;
mod stack;
mod tree_diff;
mod unchanged;

pub use arena::{Atom, List, Syntax, SyntaxArena, SyntaxId};
pub use content_id::ContentId;
pub use line_diff::diff_lines;
pub use lower::lower_tree;
use std::ops::Range;
pub use tree_diff::diff_with_language;
pub use unchanged::{
    mark_unchanged, ChangeKind as PreprocessChangeKind, ChangeMap, PreprocessResult,
};

/// Side of a structural diff that a [`DiffChange`] applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// Left-hand side, typically the base / "before" version.
    Lhs,
    /// Right-hand side, typically the buffer / "after" version.
    Rhs,
}

/// Reason a region was flagged as changed.
///
/// Mirrors `difftastic::diff::changes::ChangeKind`. The structural-diff
/// path also distinguishes "moved" subtrees, but the line-diff fallback
/// only emits these two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Bytes added on this side that have no counterpart on the other.
    Novel,
    /// Bytes that replaced corresponding bytes on the other side.
    Replaced,
}

/// One contiguous changed region of one side of the diff. Byte offsets are
/// relative to that side's input rope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffChange {
    pub side: Side,
    pub byte_range: Range<usize>,
    pub kind: ChangeKind,
}

/// Result of [`diff`] / [`diff_lines`]. `fell_back_to_line_diff` is `true`
/// for the line-diff path so callers can surface that the structural
/// algorithm was not used (either by design or due to a graph-cap fall
/// through).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffResult {
    pub changes: Vec<DiffChange>,
    pub fell_back_to_line_diff: bool,
}

/// Public entry point for callers without a language. Routes to the
/// line-diff fallback. Use [`diff_with_language_or_lines`] when a
/// language is available to get structural-quality output.
pub fn diff(lhs: &str, rhs: &str) -> DiffResult {
    diff_lines(lhs, rhs)
}

/// Best-quality diff for callers that have a [`Language`].
///
/// Tries the structural path first ([`diff_with_language`]), falling
/// through to [`diff_lines`] when either side fails to parse. The
/// `fell_back_to_line_diff` flag on the returned [`DiffResult`]
/// distinguishes the two paths so the host can surface "diff is
/// approximate" badging when the parse failed.
pub fn diff_with_language_or_lines(
    language: &std::sync::Arc<crate::Language>,
    lhs: &str,
    rhs: &str,
) -> DiffResult {
    match diff_with_language(language, lhs, rhs) {
        Some(result) => result,
        None => diff_lines(lhs, rhs),
    }
}
