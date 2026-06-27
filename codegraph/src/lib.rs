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
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};
use stoat_language::{RefKind, SymbolKind};

mod build;

pub use build::build_shard;

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

    /// Resolve an edge target against the name index.
    ///
    /// A [`Target::Sym`] is already resolved. A [`Target::Unresolved`] call
    /// is matched by name against the indexed functions and methods. A
    /// unique hit returns [`Confidence::Resolved`] with its key, several
    /// candidates return [`Confidence::Ambiguous`], and no match returns
    /// [`Confidence::NameMatch`] with no key so the target stays unresolved.
    pub fn resolve_target(&self, target: &Target) -> (Confidence, Option<SymbolKey>) {
        let name = match target {
            Target::Sym(key) => return (Confidence::Resolved, Some(*key)),
            Target::Unresolved { name, .. } => name,
        };

        let mut candidates = self.callable_candidates(name);
        match candidates.len() {
            1 => (Confidence::Resolved, Some(candidates.remove(0))),
            0 => (Confidence::NameMatch, None),
            _ => (Confidence::Ambiguous, None),
        }
    }

    fn callable_candidates(&self, name: &str) -> Vec<SymbolKey> {
        let mut out = Vec::new();
        for kind in [SymbolKind::Function, SymbolKind::Method] {
            if let Some(keys) = self.by_name.get(&(name.to_string(), kind)) {
                out.extend(keys.iter().copied());
            }
        }
        out
    }

    /// Merge one file's [`FileShard`] into the graph.
    ///
    /// Symbols are registered first so a call within the shard resolves
    /// immediately. Each edge is then resolved against the name index. An
    /// edge that resolves to a unique symbol is rewritten to [`Target::Sym`]
    /// and linked into the adjacency, while one whose target is not yet
    /// known stays unresolved for [`Self::reresolve_unresolved`] to link.
    pub fn insert_shard(&mut self, shard: FileShard) {
        for sym in shard.symbols {
            let key = sym.key;
            self.by_name
                .entry((sym.name.clone(), sym.kind))
                .or_default()
                .push(key);
            self.by_file.entry(sym.file).or_default().push(key);
            self.symbols.insert(key, sym);
        }

        for mut edge in shard.edges {
            let (confidence, resolved) = self.resolve_target(&edge.to);
            edge.confidence = confidence;
            if let Some(key) = resolved {
                edge.to = Target::Sym(key);
                let idx = self.edges.len() as u32;
                self.out.entry(edge.from).or_default().push(idx);
                self.inn.entry(key).or_default().push(idx);
            }
            self.edges.push(edge);
        }
    }

    /// Remove a file's symbols and edges from the graph.
    ///
    /// Edges originating in the file are dropped. A surviving edge from
    /// another file that pointed at one of the removed symbols is degraded
    /// back to [`Target::Unresolved`] so it can re-link to a future
    /// definition rather than dangle at a missing key.
    pub fn evict_file(&mut self, file: FileId) {
        let Some(keys) = self.by_file.remove(&file) else {
            return;
        };
        let evicted: HashSet<SymbolKey> = keys.iter().copied().collect();
        let evicted_names: HashMap<SymbolKey, String> = keys
            .iter()
            .filter_map(|k| self.symbols.get(k).map(|s| (*k, s.name.clone())))
            .collect();

        for edge in &mut self.edges {
            if evicted.contains(&edge.from) {
                continue;
            }
            if let Target::Sym(key) = edge.to
                && let Some(name) = evicted_names.get(&key)
            {
                edge.to = Target::Unresolved {
                    name: name.clone(),
                    kind: RefKind::Call,
                };
                edge.confidence = Confidence::NameMatch;
            }
        }

        self.edges.retain(|edge| !evicted.contains(&edge.from));
        for key in &keys {
            if let Some(sym) = self.symbols.remove(key) {
                remove_name_entry(&mut self.by_name, &sym.name, sym.kind, *key);
            }
        }

        self.rebuild_adjacency();
    }

    /// Re-link every still-unresolved edge against the current name index.
    ///
    /// Insertion order does not affect the final graph. A call inserted
    /// before its definition is left unresolved, and this pass resolves it
    /// once the defining shard is present.
    pub fn reresolve_unresolved(&mut self) {
        let updates: Vec<(usize, SymbolKey, Confidence)> = self
            .edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| matches!(edge.to, Target::Unresolved { .. }))
            .filter_map(|(idx, edge)| {
                let (confidence, key) = self.resolve_target(&edge.to);
                key.map(|key| (idx, key, confidence))
            })
            .collect();

        for (idx, key, confidence) in updates {
            self.edges[idx].to = Target::Sym(key);
            self.edges[idx].confidence = confidence;
        }

        self.rebuild_adjacency();
    }

    /// Rebuild `out`/`inn` from the current edges, indexing only edges
    /// whose target resolves to a present symbol.
    fn rebuild_adjacency(&mut self) {
        self.out.clear();
        self.inn.clear();
        for (idx, edge) in self.edges.iter().enumerate() {
            if let Target::Sym(key) = edge.to
                && self.symbols.contains_key(&key)
            {
                self.out.entry(edge.from).or_default().push(idx as u32);
                self.inn.entry(key).or_default().push(idx as u32);
            }
        }
    }
}

/// Remove `key` from the `(name, kind)` bucket, dropping the bucket when
/// it empties so a stale name never lingers in the index.
fn remove_name_entry(
    by_name: &mut HashMap<(String, SymbolKind), SmallVec<[SymbolKey; 2]>>,
    name: &str,
    kind: SymbolKind,
    key: SymbolKey,
) {
    let bucket_key = (name.to_string(), kind);
    if let Some(keys) = by_name.get_mut(&bucket_key) {
        keys.retain(|k| *k != key);
        if keys.is_empty() {
            by_name.remove(&bucket_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_shard, CodeGraph, Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey,
        Target,
    };
    use stoat_language::{
        extract_references, extract_symbols, parse_rope, LanguageRegistry, RefKind, SymbolKind,
    };
    use stoat_text::Rope;

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

    fn shard_of(file: FileId, file_rel: &str, text: &str) -> FileShard {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let rope = Rope::from(text);
        let tree = parse_rope(rust, &rope, None).unwrap();
        let defs = extract_symbols(
            rust.outline_query.as_ref().unwrap(),
            tree.root_node(),
            &rope,
        );
        let refs = extract_references(rust.tags_query.as_ref().unwrap(), tree.root_node(), &rope);
        build_shard(file, file_rel, [0u8; 32], text, defs, refs)
    }

    fn call_edge(graph: &CodeGraph) -> Edge {
        graph
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Calls)
            .cloned()
            .unwrap()
    }

    #[test]
    fn cross_file_call_resolves_after_reresolve_then_degrades_on_evict() {
        let a = shard_of(FileId(0), "a.rs", "fn a() {\n    foo();\n}\n");
        let b = shard_of(FileId(1), "b.rs", "fn foo() {}\n");

        let mut graph = CodeGraph::new();
        graph.insert_shard(a);
        graph.insert_shard(b);
        graph.reresolve_unresolved();

        let foo_key = graph
            .by_name
            .get(&("foo".to_string(), SymbolKind::Function))
            .and_then(|keys| keys.first().copied())
            .unwrap();
        let resolved = call_edge(&graph);
        assert_eq!(resolved.to, Target::Sym(foo_key));
        assert_eq!(resolved.confidence, Confidence::Resolved);
        assert_eq!(
            graph.inn.get(&foo_key).map(|edges| edges.as_slice()),
            Some(&[0u32][..])
        );

        graph.evict_file(FileId(1));
        let degraded = call_edge(&graph);
        assert_eq!(
            degraded.to,
            Target::Unresolved {
                name: "foo".to_string(),
                kind: RefKind::Call,
            }
        );
        assert_eq!(degraded.confidence, Confidence::NameMatch);
        assert!(!graph.symbols.contains_key(&foo_key));
    }
}
