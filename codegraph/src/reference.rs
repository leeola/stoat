//! The whole-graph sweeps the incremental reindex replaced, kept as an oracle.
//!
//! Binding is order-dependent by design. [`CodeGraph::insert_shard`] resolves
//! each edge against the name index as it merges, so an ambiguous name binds to
//! whichever definition was present first. A graph rebuilt from scratch
//! therefore legitimately disagrees with one reached by a sequence of edits, and
//! comparing against one would be measuring the wrong thing.
//!
//! What must hold is that the same sequence of edits produces the same graph
//! whichever algorithm walks it. These are the sweeps as they were before the
//! per-file and per-name indexes, so the differential below is against the
//! behaviour that shipped rather than against a fresh build.
//!
//! [`CodeGraph::apply_reindex`] merges in place and keeps the binding of every
//! edge an edit keeps, where a sweep evicts and binds afresh. Its oracle,
//! [`CodeGraph::reference_apply_reindex`], records those bindings, sweeps, and
//! gives them back.

use crate::{ref_kind_for, CodeGraph, Confidence, EdgeKind, FileId, FileShard, SymbolKey, Target};
use std::collections::{HashMap, HashSet, VecDeque};

/// What the oracle pairs a file's prior edge with a new one by, as
/// [`CodeGraph::apply_reindex`] pairs them.
#[derive(PartialEq, Eq, Hash)]
struct Paired {
    from: SymbolKey,
    kind: EdgeKind,
    target: PairedTarget,
}

#[derive(PartialEq, Eq, Hash)]
enum PairedTarget {
    Key(SymbolKey),
    Name(String),
}

impl CodeGraph {
    /// Re-index one file by sweeping the whole graph, as the shipped code did.
    pub(crate) fn reference_reindex(&mut self, file: FileId, shard: FileShard) {
        self.reference_evict_inner(file);
        self.insert_shard(shard);
        self.reference_reresolve_inner();
        self.reference_rebuild_adjacency();
    }

    /// Merge one file's new shard by sweeping, landing where
    /// [`CodeGraph::apply_reindex`] lands.
    ///
    /// The merge keeps the binding of every edge the edit keeps. Those are the
    /// file's edges whose source, kind, and target the shard keeps, and the
    /// edges from other files into a symbol the shard keeps. A sweep evicts and
    /// binds afresh, so this records those bindings, sweeps, and gives them
    /// back.
    pub(crate) fn reference_apply_reindex(&mut self, file: FileId, shard: FileShard) {
        let prior: HashSet<SymbolKey> = self
            .by_file
            .get(&file)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let kept: HashSet<SymbolKey> = shard
            .symbols
            .iter()
            .map(|sym| sym.key)
            .filter(|key| prior.contains(key))
            .collect();

        // The edges of other files keep their ids through the sweep.
        let incoming: Vec<(u32, Target, Confidence)> = self
            .live_edges()
            .filter(|(_, edge)| !prior.contains(&edge.from))
            .filter(|(_, edge)| matches!(&edge.to, Target::Sym(key) if kept.contains(key)))
            .map(|(id, edge)| (id, edge.to.clone(), edge.confidence))
            .collect();

        // The file's edges as the merge pairs them, in site order, once a
        // dropped symbol has degraded the edges into it.
        let mut held: HashMap<Paired, VecDeque<(Target, Confidence)>> = HashMap::new();
        let mut own: Vec<u32> = self.edges_by_file.get(&file).cloned().unwrap_or_default();
        own.sort_by_key(|&id| self.edge(id).map_or(0, |edge| edge.site_range.start));
        for id in own {
            let Some(edge) = self.edge(id) else {
                continue;
            };
            let degraded = match &edge.to {
                Target::Sym(key) if prior.contains(key) && !kept.contains(key) => {
                    ref_kind_for(edge.kind).map(|kind| Target::Unresolved {
                        name: self.symbols[key].name.clone(),
                        kind,
                    })
                },
                _ => None,
            };
            let (to, confidence) = match degraded {
                Some(to) => (to, Confidence::NameMatch),
                None => (edge.to.clone(), edge.confidence),
            };
            if let Some(key) = self.paired(edge.from, edge.kind, &to) {
                held.entry(key).or_default().push_back((to, confidence));
            }
        }

        self.reference_evict_inner(file);
        self.insert_shard(shard);

        for (id, to, confidence) in incoming {
            let edge = self.edges[id as usize]
                .as_mut()
                .expect("an edge of another file outlives the sweep");
            edge.to = to;
            edge.confidence = confidence;
        }

        let fresh: Vec<u32> = self.edges_by_file.get(&file).cloned().unwrap_or_default();
        for id in fresh {
            let Some(key) = self
                .edge(id)
                .and_then(|edge| self.paired(edge.from, edge.kind, &edge.to))
            else {
                continue;
            };
            if let Some((to, confidence)) = held.get_mut(&key).and_then(VecDeque::pop_front) {
                let edge = self.edges[id as usize]
                    .as_mut()
                    .expect("a fresh edge is live");
                edge.to = to;
                edge.confidence = confidence;
            }
        }

        self.reference_rebuild_adjacency();
    }

    /// What an edge from `from` of `kind` to `to` pairs by, or `None` for a
    /// resolved target whose symbol is gone.
    fn paired(&self, from: SymbolKey, kind: EdgeKind, to: &Target) -> Option<Paired> {
        let target = match (to, kind) {
            (Target::Sym(key), EdgeKind::Contains) => PairedTarget::Key(*key),
            (Target::Sym(key), _) => PairedTarget::Name(self.symbols.get(key)?.name.clone()),
            (Target::Unresolved { name, .. }, _) => PairedTarget::Name(name.clone()),
        };
        Some(Paired { from, kind, target })
    }

    /// Remove one file by sweeping the whole graph, as the shipped code did.
    pub(crate) fn reference_evict_file(&mut self, file: FileId) {
        self.reference_evict_inner(file);
        self.reference_rebuild_adjacency();
    }

    /// Re-link every unresolved edge by sweeping the whole graph.
    pub(crate) fn reference_reresolve(&mut self) {
        self.reference_reresolve_inner();
        self.reference_rebuild_adjacency();
    }

    pub(crate) fn reference_evict_inner(&mut self, file: FileId) {
        self.content_hashes.remove(&file);
        // Dropped rather than used. The sweep below finds the same edges by
        // walking, but leaving these indexes behind would let them name slots a
        // later insert has reused.
        self.edges_by_file.remove(&file);
        self.unresolved_by_name.clear();
        self.dirty_names.clear();
        let Some(keys) = self.by_file.remove(&file) else {
            return;
        };
        let evicted: HashSet<SymbolKey> = keys.iter().copied().collect();
        let evicted_names: HashMap<SymbolKey, String> = keys
            .iter()
            .filter_map(|k| self.symbols.get(k).map(|s| (*k, s.name.clone())))
            .collect();

        for edge in self.edges.iter_mut().flatten() {
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

        for id in 0..self.edges.len() as u32 {
            if self
                .edge(id)
                .is_some_and(|edge| evicted.contains(&edge.from))
            {
                self.edges[id as usize] = None;
                self.free_edges.push(id);
            }
        }

        for key in &keys {
            if let Some(sym) = self.symbols.remove(key) {
                crate::remove_name_entry(&mut self.by_name, &sym.name, *key);
            }
        }
    }

    fn reference_reresolve_inner(&mut self) {
        let updates: Vec<(u32, SymbolKey, Confidence)> = self
            .live_edges()
            .filter(|(_, edge)| matches!(edge.to, Target::Unresolved { .. }))
            .filter_map(|(id, edge)| {
                let (confidence, key) = self.resolve_target(&edge.to);
                key.map(|key| (id, key, confidence))
            })
            .collect();

        for (id, key, confidence) in updates {
            let edge = self.edges[id as usize]
                .as_mut()
                .expect("resolving a live edge");

            edge.to = Target::Sym(key);
            edge.confidence = confidence;
        }
    }

    fn reference_rebuild_adjacency(&mut self) {
        self.out.clear();
        self.inn.clear();

        let linked: Vec<(u32, SymbolKey)> = self
            .live_edges()
            .filter_map(|(id, edge)| match edge.to {
                Target::Sym(key) if self.symbols.contains_key(&key) => Some((id, key)),
                _ => None,
            })
            .collect();

        for (id, key) in linked {
            let from = self.edges[id as usize]
                .as_ref()
                .expect("linking a live edge")
                .from;

            self.out.entry(from).or_default().push(id);
            self.inn.entry(key).or_default().push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{build_shard, CodeGraph, FileId, FileShard};
    use stoat_language::{
        extract_references, extract_symbols, parse_rope, LanguageRegistry, SymbolKind,
    };
    use stoat_text::Rope;

    /// A graph's content with every edge id erased.
    ///
    /// The two paths hand out ids in different orders, and an id means nothing
    /// outside the graph that issued it, so a comparison that saw them would
    /// report differences that are not there. Everything is rendered through
    /// `Debug` and sorted, which makes the comparison total without needing an
    /// ordering on each type.
    #[derive(Debug, PartialEq, Eq)]
    struct Canonical {
        symbols: Vec<String>,
        edges: Vec<String>,
        out: Vec<(String, Vec<String>)>,
        inn: Vec<(String, Vec<String>)>,
        by_name: Vec<(String, Vec<String>)>,
        by_file: Vec<(u32, Vec<String>)>,
        content_hashes: Vec<(u32, [u8; 32])>,
    }

    fn canonical(graph: &CodeGraph) -> Canonical {
        let sorted = |mut items: Vec<String>| {
            items.sort();
            items
        };
        // Adjacency is compared by the edges it reaches, since which id holds an
        // edge is exactly what differs between the two paths.
        let adjacency =
            |map: &std::collections::HashMap<crate::SymbolKey, smallvec::SmallVec<[u32; 4]>>| {
                let mut entries: Vec<(String, Vec<String>)> = map
                    .iter()
                    .map(|(key, ids)| {
                        let edges = sorted(
                            ids.iter()
                                .map(|&id| format!("{:?}", graph.edge(id)))
                                .collect(),
                        );
                        (format!("{key:?}"), edges)
                    })
                    .collect();
                entries.sort();
                entries
            };

        let mut by_name: Vec<(String, Vec<String>)> = graph
            .by_name
            .iter()
            .map(|(name, entries)| {
                (
                    name.clone(),
                    sorted(entries.iter().map(|e| format!("{e:?}")).collect()),
                )
            })
            .collect();
        by_name.sort();

        let mut by_file: Vec<(u32, Vec<String>)> = graph
            .by_file
            .iter()
            .map(|(file, keys)| {
                // Left in its own order, which `symbol_at` binary-searches.
                (file.0, keys.iter().map(|k| format!("{k:?}")).collect())
            })
            .collect();
        by_file.sort();

        let mut content_hashes: Vec<(u32, [u8; 32])> = graph
            .content_hashes
            .iter()
            .map(|(file, hash)| (file.0, *hash))
            .collect();
        content_hashes.sort();

        Canonical {
            symbols: sorted(graph.symbols.values().map(|s| format!("{s:?}")).collect()),
            edges: sorted(
                graph
                    .live_edges()
                    .map(|(_, edge)| format!("{edge:?}"))
                    .collect(),
            ),
            out: adjacency(&graph.out),
            inn: adjacency(&graph.inn),
            by_name,
            by_file,
            content_hashes,
        }
    }

    /// A shard builder holding the language registry, since constructing one
    /// compiles every query and the driver builds hundreds of shards.
    struct Shards(LanguageRegistry);

    impl Shards {
        fn new() -> Self {
            Shards(LanguageRegistry::standard())
        }

        fn of(&self, file: FileId, file_rel: &str, text: &str) -> FileShard {
            let rust = self
                .0
                .languages()
                .iter()
                .find(|l| l.name == "rust")
                .expect("rust language");
            let rope = Rope::from(text);
            let tree = parse_rope(rust, &rope, None).expect("parse");
            let defs =
                extract_symbols(rust.outline_query().unwrap(), tree.root_node(), &rope, None);
            let refs =
                extract_references(rust.tags_query().unwrap(), tree.root_node(), &rope, None);
            build_shard(file, file_rel, [0u8; 32], text, defs, refs)
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// A handful of names shared across files, so the same name is defined in
    /// more than one place and calls to it are genuinely ambiguous. That is the
    /// case where an order-dependent binding can be observed at all.
    const NAMES: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
    const FILES: u32 = 4;

    /// One file's source, defining a subset of the shared names and calling
    /// another subset, including names no file defines.
    fn source(rng: &mut Rng) -> String {
        let mut text = String::new();
        for name in NAMES {
            if rng.below(2) == 0 {
                text.push_str(&format!("fn {name}() {{}}\n"));
            }
        }

        text.push_str("fn driver() {\n");
        for name in NAMES {
            if rng.below(2) == 0 {
                text.push_str(&format!("    {name}();\n"));
            }
        }
        text.push_str("}\n");
        text
    }

    /// The incremental path and the sweeps must reach the same graph from the
    /// same sequence of edits, which is the whole contract the rewrite has to
    /// keep. A fresh build is not the oracle: an ambiguous call binds to
    /// whichever definition merged first, so edit order is part of the answer.
    #[test]
    fn the_incremental_path_lands_where_the_sweeps_do() {
        let shards = Shards::new();

        for seed in 1..=16u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut fast = CodeGraph::new();
            let mut slow = CodeGraph::new();

            for step in 0..24 {
                let file = FileId(rng.below(FILES as usize) as u32);
                let rel = format!("f{}.rs", file.0);

                if rng.below(4) == 0 {
                    fast.apply_remove(file);
                    fast.reresolve_unresolved();
                    slow.reference_evict_file(file);
                    slow.reference_reresolve();
                } else {
                    let shard = shards.of(file, &rel, &source(&mut rng));
                    fast.reindex(file, shard.clone());
                    slow.reference_reindex(file, shard);
                }

                assert_eq!(
                    canonical(&fast),
                    canonical(&slow),
                    "seed {seed} step {step} diverged on {rel}"
                );
            }
        }
    }

    /// The application applies a whole drain of index updates before resolving
    /// once, so the adjacency an eviction or a merge consults is whatever the
    /// last resolve left plus what the applies since have linked. That is the
    /// shape the incremental eviction and the in-place merge have to be right
    /// under, and it is the only one the editor ever uses.
    #[test]
    fn a_batch_of_applies_lands_where_the_sweeps_do() {
        let shards = Shards::new();

        for seed in 1..=16u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
            let mut fast = CodeGraph::new();
            let mut slow = CodeGraph::new();

            for round in 0..8 {
                // Several files change before anything re-resolves, so an
                // eviction mid-batch sees an adjacency nothing has rebuilt.
                for _ in 0..=rng.below(3) {
                    let file = FileId(rng.below(FILES as usize) as u32);
                    let rel = format!("f{}.rs", file.0);

                    if rng.below(4) == 0 {
                        fast.apply_remove(file);
                        slow.reference_evict_inner(file);
                    } else {
                        let shard = shards.of(file, &rel, &source(&mut rng));
                        fast.apply_reindex(file, shard.clone());
                        slow.reference_apply_reindex(file, shard);
                    }
                }

                fast.reresolve_unresolved();
                slow.reference_reresolve();

                assert_eq!(
                    canonical(&fast),
                    canonical(&slow),
                    "seed {seed} round {round} diverged"
                );
            }
        }
    }

    /// Repeated calls to one name pair in site order around an edit, as the
    /// sweep pairs them.
    ///
    /// The two calls hold different bindings. The first stays unresolved from a
    /// time the name was ambiguous, and the second bound once the name was
    /// unique. The edit
    /// adds calls before and between them, so the suffix that matches by
    /// position holds a call whose name the middle also holds. Pairing that
    /// suffix call by position moves the binding to another site.
    #[test]
    fn repeated_calls_pair_in_site_order_around_an_edit() {
        let shards = Shards::new();
        let mut fast = CodeGraph::new();
        let mut slow = CodeGraph::new();
        let mut apply = |file: u32, text: Option<&str>| {
            let file = FileId(file);
            match text {
                Some(text) => {
                    let shard = shards.of(file, &format!("f{}.rs", file.0), text);
                    fast.apply_reindex(file, shard.clone());
                    slow.reference_apply_reindex(file, shard);
                },
                None => {
                    fast.apply_remove(file);
                    slow.reference_evict_inner(file);
                },
            }
        };

        apply(0, Some("fn a() {}\n"));
        apply(2, Some("fn a() {}\n"));
        apply(1, Some("fn driver() {\n    a();\n}\n"));
        apply(2, None);
        apply(1, Some("fn driver() {\n    a();\n    a();\n}\n"));
        apply(2, Some("fn a() {}\n"));
        apply(
            1,
            Some("fn driver() {\n    a();\n    b();\n    a();\n    a();\n}\n"),
        );

        fast.reresolve_unresolved();
        slow.reference_reresolve();
        assert_eq!(
            canonical(&fast),
            canonical(&slow),
            "the bound call keeps its place among the calls to its name"
        );
    }

    /// A key that two items share merges as the sweep does, with one name entry
    /// per occurrence.
    ///
    /// Two inherent `impl` blocks on one type share the impl's key. Dropping one
    /// leaves the key in the shard once, and adding it back puts it there twice
    /// again, so the merge has to follow the count.
    #[test]
    fn a_key_two_items_share_merges_as_the_sweep_does() {
        let shards = Shards::new();
        let two = "struct Foo;\nimpl Foo {\n    fn a() {}\n}\nimpl Foo {\n    fn b() {}\n}\n\
                   fn driver() {\n    a();\n}\n";
        let one = "struct Foo;\nimpl Foo {\n    fn a() {}\n}\nfn driver() {\n    a();\n}\n";
        let mut fast = CodeGraph::new();
        let mut slow = CodeGraph::new();

        for (step, text) in [two, one, two].into_iter().enumerate() {
            let shard = shards.of(FileId(0), "f0.rs", text);
            fast.apply_reindex(FileId(0), shard.clone());
            slow.reference_apply_reindex(FileId(0), shard);
            fast.reresolve_unresolved();
            slow.reference_reresolve();

            let impls = fast.by_name["Foo"]
                .iter()
                .filter(|(kind, _)| *kind == SymbolKind::Impl)
                .map(|(_, key)| *key)
                .collect::<Vec<_>>();
            assert_eq!(
                (impls.len(), impls.windows(2).all(|pair| pair[0] == pair[1])),
                (if text == two { 2 } else { 1 }, true),
                "step {step}: the impl key is in the name index once per block"
            );
            assert_eq!(canonical(&fast), canonical(&slow), "step {step} diverged");
        }
    }
}
