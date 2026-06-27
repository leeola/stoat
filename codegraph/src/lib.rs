//! In-RAM symbol-and-call graph model.
//!
//! [`CodeGraph`] holds the structures the navigation hot path reads. It
//! contains a symbol table, the edge list, forward (`out`) and reverse
//! (`inn`) adjacency, and name and file indexes.
//!
//! [`FileShard`] is the per-file unit that persists to disk, with
//! [`Symbol`] and [`Edge`] as its leaves.
//!
//! Edges are [`EdgeKind::Calls`] and [`EdgeKind::Contains`] only in v1.
//! The kind taxonomy and the [`Confidence`] scale stay open so a later
//! scope-aware or LSP pass can add richer edges without a reindex.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::{collections::HashMap, ops::Range};
use stoat_language::{RefKind, SymbolKind};

/// A workspace-relative file, interned to a small integer id.
///
/// The id is meaningful only within the [`CodeGraph`] that assigned it.
/// It is re-derived when shards reload, not stable across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(pub u32);

/// A content-addressed symbol identity, formed as a truncated blake3
/// hash over the symbol's file, container path, name, and kind.
///
/// Stable across reindexes while those inputs are unchanged, so an edge
/// can name its target by key rather than by a pointer into one specific
/// graph generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolKey(pub [u8; 16]);

/// One named definition in the graph, such as a function, type, or
/// field, with the location and metadata needed to navigate to it.
///
/// `body_hash` fingerprints the definition's source range so an
/// incremental reindex can tell whether a symbol's body changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub key: SymbolKey,
    pub file: FileId,
    pub name: String,
    pub kind: SymbolKind,
    pub container: Vec<String>,
    pub def_range: Range<usize>,
    pub name_range: Range<usize>,
    pub body_hash: [u8; 32],
}

/// The relationship an [`Edge`] encodes.
///
/// Only calls and containment are modeled in v1. The set stays open for
/// richer edge kinds later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Contains,
}

/// How firmly an [`Edge`]'s target is resolved.
///
/// v1 resolves by tree-sitter name match. [`Confidence::Resolved`] is a
/// unique name hit, [`Confidence::Ambiguous`] a name with several
/// candidates, and [`Confidence::NameMatch`] the unrefined default. The
/// scale leaves room for a later scope-aware or LSP pass to upgrade edges
/// in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    NameMatch,
    Ambiguous,
    Resolved,
}

/// The destination of an [`Edge`], either a resolved [`SymbolKey`] or an
/// unresolved name kept verbatim for a later resolution pass.
///
/// Cross-file edges stay name-addressed via [`Target::Unresolved`] so a
/// shard merges and rebuilds independently of other files' internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Sym(SymbolKey),
    Unresolved { name: String, kind: RefKind },
}

/// A directed relationship from one symbol to a [`Target`].
///
/// `site_range` is the byte range of the reference that produced the edge
/// (e.g. the call site), so navigation can jump to the exact use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: SymbolKey,
    pub to: Target,
    pub kind: EdgeKind,
    pub site_range: Range<usize>,
    pub confidence: Confidence,
}

/// The persisted unit of the index, holding every symbol and edge
/// extracted from one file plus the content hash that produced them.
///
/// Shards persist and reload independently. `content_hash` lets an
/// incremental reindex skip a file whose contents are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileShard {
    pub content_hash: [u8; 32],
    pub symbols: Vec<Symbol>,
    pub edges: Vec<Edge>,
}

/// The in-RAM graph the navigation hot path reads.
///
/// Forward (`out`) and reverse (`inn`) adjacency map a symbol to the
/// indices of its outgoing and incoming edges in `edges`. `by_name`
/// indexes symbols by name and kind for name-match resolution, and
/// `by_file` groups them per file for incremental eviction.
///
/// It is not serialized. It is rebuilt in RAM by merging [`FileShard`]s,
/// which are what persist.
// FIXME: drop this allow once the merge and query methods that read these
// index fields are implemented.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct CodeGraph {
    symbols: HashMap<SymbolKey, Symbol>,
    edges: Vec<Edge>,
    out: HashMap<SymbolKey, SmallVec<[u32; 4]>>,
    inn: HashMap<SymbolKey, SmallVec<[u32; 4]>>,
    by_name: HashMap<(String, SymbolKind), SmallVec<[SymbolKey; 2]>>,
    by_file: HashMap<FileId, Vec<SymbolKey>>,
}

impl CodeGraph {
    /// Create an empty graph. Populate it by merging [`FileShard`]s.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
    use stoat_language::{RefKind, SymbolKind};

    #[test]
    fn file_shard_round_trips_through_postcard() {
        let shard = FileShard {
            content_hash: [7u8; 32],
            symbols: vec![Symbol {
                key: SymbolKey([1u8; 16]),
                file: FileId(3),
                name: "foo".to_string(),
                kind: SymbolKind::Function,
                container: vec!["m".to_string()],
                def_range: 0..10,
                name_range: 3..6,
                body_hash: [9u8; 32],
            }],
            edges: vec![Edge {
                from: SymbolKey([1u8; 16]),
                to: Target::Unresolved {
                    name: "bar".to_string(),
                    kind: RefKind::Call,
                },
                kind: EdgeKind::Calls,
                site_range: 4..7,
                confidence: Confidence::NameMatch,
            }],
        };

        let bytes = postcard::to_allocvec(&shard).unwrap();
        let decoded: FileShard = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, shard);
    }
}
