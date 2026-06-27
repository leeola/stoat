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
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
};
use stoat_language::{RefKind, SymbolKind};

mod build;
mod codec;

pub use build::build_shard;
pub use codec::{
    decode_manifest, decode_shard, encode_manifest, encode_shard, FileEntry, Manifest,
    SCHEMA_VERSION,
};

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
/// [`EdgeKind::Calls`] is a call site, [`EdgeKind::References`] a
/// type-position use, [`EdgeKind::Implements`] an `impl Trait for Type`
/// relation from the impl to the trait, and [`EdgeKind::Contains`] the
/// structural nesting of a definition inside another. The set stays open for
/// richer edge kinds later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    References,
    Implements,
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

/// Which way to walk an edge axis during traversal.
///
/// [`Dir::Up`] follows edges in reverse to their sources, such as the
/// callers of a function or the container of a symbol. [`Dir::Down`]
/// follows them to their targets, the callees or contained symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
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
    content_hashes: HashMap<FileId, [u8; 32]>,
}

impl CodeGraph {
    /// Create an empty graph. Populate it by merging [`FileShard`]s.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve an edge target against the name index.
    ///
    /// A [`Target::Sym`] is already resolved. A [`Target::Unresolved`] is
    /// matched by name against the symbol kinds its [`RefKind`] can name: a
    /// call against functions and methods, a type reference against structs,
    /// enums, traits, and type aliases. A unique hit returns
    /// [`Confidence::Resolved`] with its key, several candidates return
    /// [`Confidence::Ambiguous`], and no match returns [`Confidence::NameMatch`]
    /// with no key so the target stays unresolved.
    pub fn resolve_target(&self, target: &Target) -> (Confidence, Option<SymbolKey>) {
        let (name, kind) = match target {
            Target::Sym(key) => return (Confidence::Resolved, Some(*key)),
            Target::Unresolved { name, kind } => (name, *kind),
        };

        let mut candidates = self.candidates_for(name, kind);
        match candidates.len() {
            1 => (Confidence::Resolved, Some(candidates.remove(0))),
            0 => (Confidence::NameMatch, None),
            _ => (Confidence::Ambiguous, None),
        }
    }

    fn candidates_for(&self, name: &str, kind: RefKind) -> Vec<SymbolKey> {
        let symbol_kinds: &[SymbolKind] = match kind {
            RefKind::Call => &[SymbolKind::Function, SymbolKind::Method],
            RefKind::Type => &[
                SymbolKind::Struct,
                SymbolKind::Enum,
                SymbolKind::Trait,
                SymbolKind::TypeAlias,
            ],
            RefKind::Implements => &[SymbolKind::Trait],
        };

        let mut out = Vec::new();
        for &symbol_kind in symbol_kinds {
            if let Some(keys) = self.by_name.get(&(name.to_string(), symbol_kind)) {
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
        let content_hash = shard.content_hash;
        let mut files: HashSet<FileId> = HashSet::new();
        for sym in shard.symbols {
            files.insert(sym.file);
            let key = sym.key;
            self.by_name
                .entry((sym.name.clone(), sym.kind))
                .or_default()
                .push(key);
            self.by_file.entry(sym.file).or_default().push(key);
            self.symbols.insert(key, sym);
        }
        for file in &files {
            self.content_hashes.insert(*file, content_hash);
            self.sort_file_index(*file);
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
        self.content_hashes.remove(&file);
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
                && let Some(kind) = ref_kind_for(edge.kind)
            {
                edge.to = Target::Unresolved {
                    name: name.clone(),
                    kind,
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

    /// Order a file's symbol keys by definition start so [`Self::symbol_at`]
    /// can binary-search them.
    fn sort_file_index(&mut self, file: FileId) {
        let Some(mut keys) = self.by_file.remove(&file) else {
            return;
        };
        keys.sort_by_key(|key| self.symbols[key].def_range.start);
        self.by_file.insert(file, keys);
    }

    /// The innermost symbol whose definition range contains `offset` in
    /// `file`, or `None` when the offset lies in no definition.
    ///
    /// This is the cursor-to-symbol entry point for navigation. It reads
    /// only the in-RAM index, binary-searching the file's start-ordered
    /// symbols and then walking outward to the first enclosing range.
    pub fn symbol_at(&self, file: FileId, offset: usize) -> Option<SymbolKey> {
        let keys = self.by_file.get(&file)?;
        let start_of = |key: &SymbolKey| self.symbols[key].def_range.start;
        let mut hi = keys.partition_point(|key| start_of(key) <= offset);
        while hi > 0 {
            hi -= 1;
            let range = &self.symbols[&keys[hi]].def_range;
            if offset < range.end {
                return Some(keys[hi]);
            }
        }
        None
    }

    /// The symbol for `key`, or `None` when it is not in the graph.
    ///
    /// Resolves a key from [`Self::symbol_at`] or [`Self::step`] to the
    /// symbol's definition range, name, and file so navigation can open the
    /// file and place the cursor.
    pub fn symbol(&self, key: SymbolKey) -> Option<&Symbol> {
        self.symbols.get(&key)
    }

    /// The content hash last recorded for `file`, or `None` when no shard
    /// for it is present.
    ///
    /// Lets a caller detect that a file's on-disk content matches what the
    /// graph already holds and skip re-extracting it.
    pub fn content_hash(&self, file: FileId) -> Option<[u8; 32]> {
        self.content_hashes.get(&file).copied()
    }

    /// The neighbors of `from` along edges of `kind` in direction `dir`.
    ///
    /// [`Dir::Down`] yields the targets of `from`'s outgoing edges
    /// (callees, contained symbols); [`Dir::Up`] yields the sources of its
    /// incoming edges (callers, container). Only resolved edges are linked
    /// into the adjacency, so an unresolved call contributes no neighbor.
    pub fn step(&self, from: SymbolKey, kind: EdgeKind, dir: Dir) -> Vec<SymbolKey> {
        let indices = match dir {
            Dir::Up => self.inn.get(&from),
            Dir::Down => self.out.get(&from),
        };
        let Some(indices) = indices else {
            return Vec::new();
        };
        indices
            .iter()
            .filter_map(|&idx| {
                let edge = &self.edges[idx as usize];
                if edge.kind != kind {
                    return None;
                }
                match dir {
                    Dir::Up => Some(edge.from),
                    Dir::Down => match edge.to {
                        Target::Sym(key) => Some(key),
                        Target::Unresolved { .. } => None,
                    },
                }
            })
            .collect()
    }

    /// The path from `start` to the nearest symbol satisfying `pred`,
    /// walking edges of `kind` in direction `dir`, bounded to `max_depth`
    /// hops.
    ///
    /// `start` is the origin, not a candidate. `pred` is tested only on
    /// reached symbols, so navigation always moves off the cursor. The
    /// returned path runs `[start, .., match]`. `None` means no match
    /// within `max_depth`. The closure keeps any diff or filter dependency
    /// in the caller, leaving this crate IO-free.
    pub fn nearest(
        &self,
        start: SymbolKey,
        kind: EdgeKind,
        dir: Dir,
        pred: impl Fn(SymbolKey) -> bool,
        max_depth: usize,
    ) -> Option<Vec<SymbolKey>> {
        let mut parent: HashMap<SymbolKey, SymbolKey> = HashMap::from([(start, start)]);
        let mut frontier: VecDeque<(SymbolKey, usize)> = VecDeque::from([(start, 0)]);
        while let Some((node, depth)) = frontier.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for next in self.step(node, kind, dir) {
                if parent.contains_key(&next) {
                    continue;
                }
                parent.insert(next, node);
                if pred(next) {
                    return Some(reconstruct_path(&parent, start, next));
                }
                frontier.push_back((next, depth + 1));
            }
        }
        None
    }

    /// The shortest path from `a` to `b` over edges of `kind`, or `None`
    /// when none exists.
    ///
    /// Uses a bidirectional search, expanding forward from `a` over its
    /// outgoing edges and backward from `b` over its incoming ones until
    /// the two frontiers meet, which visits far fewer nodes than a
    /// one-sided search on a wide graph.
    pub fn path_between(
        &self,
        a: SymbolKey,
        b: SymbolKey,
        kind: EdgeKind,
    ) -> Option<Vec<SymbolKey>> {
        if a == b {
            return Some(vec![a]);
        }
        let mut fwd: HashMap<SymbolKey, SymbolKey> = HashMap::from([(a, a)]);
        let mut bwd: HashMap<SymbolKey, SymbolKey> = HashMap::from([(b, b)]);
        let mut fq: VecDeque<SymbolKey> = VecDeque::from([a]);
        let mut bq: VecDeque<SymbolKey> = VecDeque::from([b]);

        while !fq.is_empty() && !bq.is_empty() {
            if let Some(meet) = self.expand_frontier(&mut fq, &mut fwd, &bwd, kind, Dir::Down) {
                return Some(stitch_path(&fwd, &bwd, meet));
            }
            if let Some(meet) = self.expand_frontier(&mut bq, &mut bwd, &fwd, kind, Dir::Up) {
                return Some(stitch_path(&fwd, &bwd, meet));
            }
        }
        None
    }

    /// Expand one BFS level of `queue`, recording parents in `near` and
    /// returning the first node already reached by the `far` search.
    fn expand_frontier(
        &self,
        queue: &mut VecDeque<SymbolKey>,
        near: &mut HashMap<SymbolKey, SymbolKey>,
        far: &HashMap<SymbolKey, SymbolKey>,
        kind: EdgeKind,
        dir: Dir,
    ) -> Option<SymbolKey> {
        for _ in 0..queue.len() {
            let node = queue.pop_front()?;
            for next in self.step(node, kind, dir) {
                if near.contains_key(&next) {
                    continue;
                }
                near.insert(next, node);
                if far.contains_key(&next) {
                    return Some(next);
                }
                queue.push_back(next);
            }
        }
        None
    }
}

/// The [`RefKind`] a degraded edge of this [`EdgeKind`] re-resolves as, or
/// `None` for [`EdgeKind::Contains`].
///
/// Containment is structural rather than name-resolved, and a Contains edge's
/// source is always evicted alongside its target, so it never degrades; `None`
/// keeps the mapping total without inventing a reference kind for it.
fn ref_kind_for(kind: EdgeKind) -> Option<RefKind> {
    match kind {
        EdgeKind::Calls => Some(RefKind::Call),
        EdgeKind::References => Some(RefKind::Type),
        EdgeKind::Implements => Some(RefKind::Implements),
        EdgeKind::Contains => None,
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

/// Follow parent pointers from `end` back to `start`, returning the path
/// in forward order `[start, .., end]`.
fn reconstruct_path(
    parent: &HashMap<SymbolKey, SymbolKey>,
    start: SymbolKey,
    end: SymbolKey,
) -> Vec<SymbolKey> {
    let mut path = vec![end];
    let mut cur = end;
    while cur != start {
        cur = parent[&cur];
        path.push(cur);
    }
    path.reverse();
    path
}

/// Join the forward chain from `a` and the backward chain from `b` at
/// their shared `meet` node into one `[a, .., b]` path. The sentinel for
/// each search is the start node pointing at itself.
fn stitch_path(
    fwd: &HashMap<SymbolKey, SymbolKey>,
    bwd: &HashMap<SymbolKey, SymbolKey>,
    meet: SymbolKey,
) -> Vec<SymbolKey> {
    let mut path = vec![meet];
    let mut cur = meet;
    while fwd[&cur] != cur {
        cur = fwd[&cur];
        path.push(cur);
    }
    path.reverse();

    let mut cur = meet;
    while bwd[&cur] != cur {
        cur = bwd[&cur];
        path.push(cur);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{
        build_shard, CodeGraph, Confidence, Dir, Edge, EdgeKind, FileId, FileShard, Symbol,
        SymbolKey, Target,
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

    #[test]
    fn type_reference_resolves_to_type_not_same_named_function() {
        let mut graph = CodeGraph::new();
        graph.insert_shard(shard_of(FileId(0), "a.rs", "struct Foo;\n"));
        graph.insert_shard(shard_of(FileId(1), "b.rs", "fn Foo() {}\n"));

        let key_of = |kind| graph.by_name.get(&("Foo".to_string(), kind)).unwrap()[0];
        let struct_key = key_of(SymbolKind::Struct);
        let fn_key = key_of(SymbolKind::Function);

        let type_ref = Target::Unresolved {
            name: "Foo".to_string(),
            kind: RefKind::Type,
        };
        assert_eq!(
            graph.resolve_target(&type_ref),
            (Confidence::Resolved, Some(struct_key))
        );

        let call_ref = Target::Unresolved {
            name: "Foo".to_string(),
            kind: RefKind::Call,
        };
        assert_eq!(
            graph.resolve_target(&call_ref),
            (Confidence::Resolved, Some(fn_key))
        );
    }

    #[test]
    fn impl_emits_implements_edge_to_trait() {
        let mut graph = CodeGraph::new();
        graph.insert_shard(shard_of(
            FileId(0),
            "a.rs",
            "trait Greet {}\nstruct Point;\nimpl Greet for Point {}\n",
        ));
        graph.reresolve_unresolved();

        let greet = graph
            .by_name
            .get(&("Greet".to_string(), SymbolKind::Trait))
            .unwrap()[0];
        let impl_key = graph
            .by_name
            .get(&("Point".to_string(), SymbolKind::Impl))
            .unwrap()[0];

        let implements: Vec<(SymbolKey, Target)> = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Implements)
            .map(|e| (e.from, e.to.clone()))
            .collect();
        assert_eq!(implements, vec![(impl_key, Target::Sym(greet))]);
    }

    #[test]
    fn symbol_at_finds_innermost_definition() {
        let shard = shard_of(FileId(0), "m.rs", "mod m {\n    fn helper() {}\n}\n");
        let mut graph = CodeGraph::new();
        graph.insert_shard(shard);

        let key_of = |name: &str, kind| graph.by_name.get(&(name.to_string(), kind)).unwrap()[0];
        let helper = key_of("helper", SymbolKind::Function);
        let m = key_of("m", SymbolKind::Module);
        let helper_start = graph.symbols[&helper].def_range.start;
        let m_range = graph.symbols[&m].def_range.clone();

        assert_eq!(graph.symbol_at(FileId(0), helper_start), Some(helper));
        assert_eq!(graph.symbol_at(FileId(0), m_range.start), Some(m));
        assert_eq!(graph.symbol_at(FileId(0), m_range.end), None);
        assert_eq!(graph.symbol_at(FileId(1), helper_start), None);
    }

    #[test]
    fn content_hash_tracks_inserted_and_evicted_shards() {
        let mut graph = CodeGraph::new();
        assert_eq!(graph.content_hash(FileId(0)), None);

        graph.insert_shard(FileShard {
            content_hash: [5u8; 32],
            symbols: vec![Symbol {
                key: SymbolKey([1u8; 16]),
                file: FileId(0),
                name: "foo".to_string(),
                kind: SymbolKind::Function,
                container: vec![],
                def_range: 0..10,
                name_range: 3..6,
                body_hash: [0u8; 32],
            }],
            edges: vec![],
        });
        assert_eq!(graph.content_hash(FileId(0)), Some([5u8; 32]));

        graph.evict_file(FileId(0));
        assert_eq!(graph.content_hash(FileId(0)), None);
    }

    #[test]
    fn symbol_returns_the_symbol_for_a_key() {
        let mut graph = CodeGraph::new();
        graph.insert_shard(shard_of(FileId(0), "m.rs", "fn helper() {}\n"));

        let key = graph
            .by_name
            .get(&("helper".to_string(), SymbolKind::Function))
            .unwrap()[0];
        assert_eq!(graph.symbol(key).map(|s| s.name.as_str()), Some("helper"));
        assert!(graph.symbol(SymbolKey([9u8; 16])).is_none());
    }

    fn call_chain(n: usize) -> (CodeGraph, Vec<SymbolKey>) {
        let keys: Vec<SymbolKey> = (0..n)
            .map(|i| {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
                SymbolKey(bytes)
            })
            .collect();
        let symbols = keys
            .iter()
            .enumerate()
            .map(|(i, &key)| Symbol {
                key,
                file: FileId(0),
                name: format!("n{i}"),
                kind: SymbolKind::Function,
                container: vec![],
                def_range: i * 10..i * 10 + 5,
                name_range: i * 10..i * 10 + 2,
                body_hash: [0u8; 32],
            })
            .collect();
        let edges = (0..n.saturating_sub(1))
            .map(|i| Edge {
                from: keys[i],
                to: Target::Sym(keys[i + 1]),
                kind: EdgeKind::Calls,
                site_range: 0..1,
                confidence: Confidence::Resolved,
            })
            .collect();

        let mut graph = CodeGraph::new();
        graph.insert_shard(FileShard {
            content_hash: [0u8; 32],
            symbols,
            edges,
        });
        (graph, keys)
    }

    #[test]
    fn bounded_traversal_over_a_long_chain() {
        let (graph, k) = call_chain(10_000);

        assert_eq!(graph.step(k[10], EdgeKind::Calls, Dir::Down), vec![k[11]]);
        assert_eq!(graph.step(k[10], EdgeKind::Calls, Dir::Up), vec![k[9]]);

        let down = graph
            .nearest(
                k[0],
                EdgeKind::Calls,
                Dir::Down,
                |key| key == k[5000],
                10_000,
            )
            .unwrap();
        assert_eq!(down.first(), Some(&k[0]));
        assert_eq!(down.last(), Some(&k[5000]));
        assert_eq!(down.len(), 5001);

        assert_eq!(
            graph.nearest(k[0], EdgeKind::Calls, Dir::Down, |key| key == k[5000], 100),
            None
        );

        let between = graph.path_between(k[3], k[8], EdgeKind::Calls).unwrap();
        assert_eq!(between, (3..=8).map(|i| k[i]).collect::<Vec<_>>());

        let full = graph.path_between(k[0], k[9999], EdgeKind::Calls).unwrap();
        assert_eq!(full.len(), 10_000);

        assert_eq!(graph.path_between(k[8], k[3], EdgeKind::Calls), None);
    }
}
