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
mod moves;
mod sliders;
mod stack;
mod tree_diff;
mod unchanged;

pub use arena::{Atom, List, Syntax, SyntaxArena, SyntaxId};
pub use content_id::ContentId;
pub use line_diff::diff_lines;
pub use lower::lower_tree;
pub use moves::{find_moves, MoveRecord};
use std::{ops::Range, path::PathBuf, sync::Arc};
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
/// Mirrors `difftastic::diff::changes::ChangeKind` plus a `Moved` variant
/// that difftastic doesn't model: a syntactic subtree that exists on both
/// sides with matching [`ContentId`] but at a different relative
/// position. Detected by the post-Dijkstra move pass in [`find_moves`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Bytes added on this side that have no counterpart on the other.
    Novel,
    /// Bytes that replaced corresponding bytes on the other side.
    Replaced,
    /// Bytes that moved from (Lhs) or to (Rhs) an alternate location.
    /// Paired with a [`MoveMetadata`] on the owning [`DiffChange`] that
    /// identifies the counterpart location(s).
    Moved,
}

/// One contiguous changed region of one side of the diff. Byte offsets are
/// relative to that side's input rope. [`move_metadata`] is `Some` iff
/// [`kind`] is [`ChangeKind::Moved`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffChange {
    pub side: Side,
    pub byte_range: Range<usize>,
    pub kind: ChangeKind,
    pub move_metadata: Option<Arc<MoveMetadata>>,
    /// Pairs an [`Lhs`](Side::Lhs) `Replaced` change with its
    /// [`Rhs`](Side::Rhs) counterpart. Within a single [`DiffResult`],
    /// a given `pair_id` appears on exactly one `Lhs` entry and one
    /// `Rhs` entry. `None` for [`ChangeKind::Novel`] (pure add/delete)
    /// and [`ChangeKind::Moved`] changes, which have other pairing
    /// machinery.
    pub pair_id: Option<u32>,
    /// For [`Lhs`](Side::Lhs)-side deletions (`kind == Novel`), the
    /// [`Rhs`](Side::Rhs) line number (0-based) where the deletion
    /// logically belongs; used to anchor deleted content at the right
    /// position in the buffer view. `None` for any other kind.
    pub deletion_rhs_anchor: Option<u32>,
}

/// Provenance for a [`ChangeKind::Moved`] region. `sources` enumerates the
/// counterpart location(s) on the other side: length 1 for unambiguous
/// moves and `> 1` when multiple candidate source locations share the
/// same [`ContentId`] (consolidation from N places into one, or an
/// ambiguous N:M pairing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveMetadata {
    pub sources: Vec<MoveSource>,
}

/// One candidate source location for a [`MoveMetadata`]. `buffer` is
/// `None` for intra-file moves (both sides of the same diff refer to the
/// same logical file) and `Some` when the move crosses files inside a
/// changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveSource {
    pub buffer: Option<BufferRef>,
    pub side: Side,
    pub byte_range: Range<usize>,
    pub line_range: Range<u32>,
}

/// Identifier for the file a cross-file [`MoveSource`] points at.
/// `fingerprint` is expected to be a blake3 hash of the file's source
/// text; this crate treats it as an opaque 32-byte key and the stoat
/// workspace layer computes and owns the actual hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferRef {
    pub path: PathBuf,
    pub fingerprint: [u8; 32],
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
    language: &Arc<crate::Language>,
    lhs: &str,
    rhs: &str,
) -> DiffResult {
    match diff_with_language(language, lhs, rhs) {
        Some(result) => result,
        None => diff_lines(lhs, rhs),
    }
}
