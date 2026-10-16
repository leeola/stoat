//! Building a [`FileShard`] from one file's extracted definitions and
//! references.
//!
//! [`build_shard`] is pure: it turns the language crate's `SymbolDef`s and
//! `RefSite`s into the graph's [`Symbol`]s and [`Edge`]s, leaving call
//! targets unresolved for the merge step to link by name.

use crate::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
use std::{cmp::Reverse, ops::Range};
use stoat_language::{RefKind, RefSite, SymbolDef, SymbolKind};

/// Build the [`FileShard`] for one file from its extracted definitions
/// and references.
///
/// `text` is the file's source, used to fingerprint each symbol's body;
/// `defs` and `refs` must have been extracted from that same text so their
/// byte ranges line up. Containment edges are derived from def-range
/// nesting, and each reference becomes a [`Target::Unresolved`] call edge
/// from the definition that encloses its site.
pub fn build_shard(
    file: FileId,
    file_rel: &str,
    content_hash: [u8; 32],
    text: &str,
    defs: Vec<SymbolDef>,
    refs: Vec<RefSite>,
) -> FileShard {
    let symbols: Vec<Symbol> = defs
        .into_iter()
        .map(|d| {
            let key = symbol_key(file_rel, &d.container, &d.name, d.kind);
            let body_hash = hash32(text[d.def_range.clone()].as_bytes());
            Symbol {
                key,
                file,
                name: d.name,
                kind: d.kind,
                container: d.container,
                def_range: d.def_range,
                name_range: d.name_range,
                body_hash,
            }
        })
        .collect();

    let ranges: Vec<Range<usize>> = symbols.iter().map(|s| s.def_range.clone()).collect();
    let offsets: Vec<usize> = refs.iter().map(|r| r.site_range.start).collect();
    let mut edges = contains_edges(&symbols, &containers(&ranges));
    edges.extend(reference_edges(
        &symbols,
        &refs,
        &enclosing(&ranges, &offsets),
    ));

    FileShard {
        content_hash,
        symbols,
        edges,
    }
}

/// One [`EdgeKind::Contains`] edge per symbol that nests inside another,
/// from the innermost enclosing symbol to the nested one.
///
/// `parents` holds each symbol's innermost container, as [`containers`]
/// answers it.
fn contains_edges(symbols: &[Symbol], parents: &[Option<usize>]) -> Vec<Edge> {
    symbols
        .iter()
        .zip(parents)
        .filter_map(|(child, &parent)| {
            let parent = &symbols[parent?];
            Some(Edge {
                from: parent.key,
                to: Target::Sym(child.key),
                kind: EdgeKind::Contains,
                site_range: child.name_range.clone(),
                confidence: Confidence::Resolved,
            })
        })
        .collect()
}

/// One edge per reference whose site falls inside a definition, from that
/// definition to the unresolved referenced name.
///
/// `callers` holds the innermost definition around each reference's site, as
/// [`enclosing`] answers it.
///
/// The edge kind follows the reference kind. A call site becomes an
/// [`EdgeKind::Calls`] edge and a type-position use an [`EdgeKind::References`]
/// edge. The target carries the same [`RefKind`] so resolution stays
/// kind-aware.
fn reference_edges(symbols: &[Symbol], refs: &[RefSite], callers: &[Option<usize>]) -> Vec<Edge> {
    refs.iter()
        .zip(callers)
        .filter_map(|(r, &caller)| {
            let caller = &symbols[caller?];
            Some(Edge {
                from: caller.key,
                to: Target::Unresolved {
                    name: r.name.clone(),
                    kind: r.kind,
                },
                kind: match r.kind {
                    RefKind::Call => EdgeKind::Calls,
                    RefKind::Type => EdgeKind::References,
                    RefKind::Implements => EdgeKind::Implements,
                },
                site_range: r.site_range.clone(),
                confidence: Confidence::NameMatch,
            })
        })
        .collect()
}

/// The index of each range's innermost strict container, or `None` for a
/// range that no other range strictly contains.
///
/// Among copies of the innermost container's range, the lowest index answers.
/// The ranges must be non-empty and must nest or be disjoint, as the def
/// ranges of syntax items do. A sort and one stack pass then stand in for a
/// scan of every range per range.
fn containers(ranges: &[Range<usize>]) -> Vec<Option<usize>> {
    let mut out = vec![None; ranges.len()];
    let mut stack: Vec<usize> = Vec::new();
    for i in nesting_order(ranges) {
        while stack
            .last()
            .is_some_and(|&top| ranges[top].end < ranges[i].end)
        {
            stack.pop();
        }

        // Copies of this range sit on top, and none of them strictly holds it.
        let copies = stack
            .iter()
            .rev()
            .take_while(|&&entry| ranges[entry] == ranges[i])
            .count();
        out[i] = lowest_copy_at_top(&stack[..stack.len() - copies], ranges);
        stack.push(i);
    }
    out
}

/// The index of the innermost range that holds each offset, or `None` for an
/// offset outside every range.
///
/// Among copies of the innermost range, the lowest index answers. The ranges
/// must meet the precondition of [`containers`].
fn enclosing(ranges: &[Range<usize>], offsets: &[usize]) -> Vec<Option<usize>> {
    let mut by_offset: Vec<usize> = (0..offsets.len()).collect();
    by_offset.sort_by_key(|&k| (offsets[k], k));

    let mut out = vec![None; offsets.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut unopened = nesting_order(ranges).into_iter().peekable();
    for k in by_offset {
        let offset = offsets[k];
        while let Some(i) = unopened.next_if(|&i| ranges[i].start <= offset) {
            while stack
                .last()
                .is_some_and(|&top| ranges[top].end < ranges[i].end)
            {
                stack.pop();
            }
            stack.push(i);
        }
        while stack.last().is_some_and(|&top| ranges[top].end <= offset) {
            stack.pop();
        }
        out[k] = lowest_copy_at_top(&stack, ranges);
    }
    out
}

/// The range indices by start, then by end from the widest, then by index,
/// which is the order the stack sweeps meet them in.
fn nesting_order(ranges: &[Range<usize>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&i| (ranges[i].start, Reverse(ranges[i].end), i));
    order
}

/// The lowest index among the copies of the range at the top of `stack`.
///
/// A sweep pushes the copies of one range next to each other in index order,
/// so the deepest copy holds the lowest index.
fn lowest_copy_at_top(stack: &[usize], ranges: &[Range<usize>]) -> Option<usize> {
    let &top = stack.last()?;
    stack
        .iter()
        .rev()
        .take_while(|&&entry| ranges[entry] == ranges[top])
        .last()
        .copied()
}

/// A content-addressed key from the symbol's location and identity. A
/// null byte separates the fields so distinct field splits cannot collide.
fn symbol_key(file_rel: &str, container: &[String], name: &str, kind: SymbolKind) -> SymbolKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(file_rel.as_bytes());
    hasher.update(&[0]);
    hasher.update(container.join("::").as_bytes());
    hasher.update(&[0]);
    hasher.update(name.as_bytes());
    hasher.update(&[0, kind_tag(kind)]);

    let mut key = [0u8; 16];
    key.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    SymbolKey(key)
}

fn kind_tag(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Struct => 2,
        SymbolKind::Enum => 3,
        SymbolKind::Trait => 4,
        SymbolKind::Impl => 5,
        SymbolKind::Module => 6,
        SymbolKind::Const => 7,
        SymbolKind::Static => 8,
        SymbolKind::TypeAlias => 9,
        SymbolKind::Macro => 10,
        SymbolKind::Field => 11,
        SymbolKind::EnumVariant => 12,
    }
}

fn hash32(bytes: &[u8]) -> [u8; 32] {
    blake3::hash(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::{build_shard, containers, enclosing};
    use crate::{CodeGraph, Confidence, EdgeKind, FileId, FileShard, SymbolKey, Target};
    use std::{collections::HashMap, ops::Range};
    use stoat_language::{
        extract_references, extract_symbols, parse_rope, LanguageRegistry, SymbolKind,
    };
    use stoat_text::Rope;

    const SNIPPET: &str = "\
mod m {
    fn helper() {}
}

fn main() {
    helper();
}
";

    fn build() -> FileShard {
        build_text(SNIPPET)
    }

    fn build_text(text: &str) -> FileShard {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let rope = Rope::from(text);
        let tree = parse_rope(rust, &rope, None).unwrap();
        let defs = extract_symbols(rust.outline_query().unwrap(), tree.root_node(), &rope, None);
        let refs = extract_references(rust.tags_query().unwrap(), tree.root_node(), &rope, None);
        build_shard(FileId(0), "m.rs", [0u8; 32], text, defs, refs)
    }

    #[test]
    fn build_shard_emits_symbols_and_edges() {
        let shard = build();

        let mut symbols = shard.symbols.clone();
        symbols.sort_by_key(|s| s.def_range.start);
        let sym_view: Vec<(&str, SymbolKind, Vec<&str>)> = symbols
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.kind,
                    s.container.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            sym_view,
            vec![
                ("m", SymbolKind::Module, vec![]),
                ("helper", SymbolKind::Function, vec!["m"]),
                ("main", SymbolKind::Function, vec![]),
            ]
        );

        let name_of: HashMap<SymbolKey, &str> = shard
            .symbols
            .iter()
            .map(|s| (s.key, s.name.as_str()))
            .collect();
        let mut edge_view: Vec<(&str, EdgeKind, String)> = shard
            .edges
            .iter()
            .map(|e| {
                let to = match &e.to {
                    Target::Sym(k) => format!("sym:{}", name_of[k]),
                    Target::Unresolved { name, .. } => format!("unresolved:{name}"),
                };
                (name_of[&e.from], e.kind, to)
            })
            .collect();
        edge_view.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
        assert_eq!(
            edge_view,
            vec![
                ("m", EdgeKind::Contains, "sym:helper".to_string()),
                ("main", EdgeKind::Calls, "unresolved:helper".to_string()),
            ]
        );
    }

    #[test]
    fn self_call_resolves_within_shard() {
        let shard = build();

        let mut graph = CodeGraph::new();
        for s in &shard.symbols {
            graph
                .by_name
                .entry(s.name.clone())
                .or_default()
                .push((s.kind, s.key));
        }

        let call = shard
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Calls)
            .unwrap();
        let helper_key = shard
            .symbols
            .iter()
            .find(|s| s.name == "helper")
            .unwrap()
            .key;

        assert_eq!(
            graph.resolve_target(&call.to),
            (Confidence::Resolved, Some(helper_key))
        );
    }

    #[test]
    fn type_use_emits_references_edges() {
        let shard = build_text("fn f(p: Point) -> Dir {}");

        let name_of: HashMap<SymbolKey, &str> = shard
            .symbols
            .iter()
            .map(|s| (s.key, s.name.as_str()))
            .collect();
        let mut edges: Vec<(&str, EdgeKind, String)> = shard
            .edges
            .iter()
            .map(|e| {
                let to = match &e.to {
                    Target::Sym(k) => format!("sym:{}", name_of[k]),
                    Target::Unresolved { name, .. } => format!("unresolved:{name}"),
                };
                (name_of[&e.from], e.kind, to)
            })
            .collect();
        edges.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
        assert_eq!(
            edges,
            vec![
                ("f", EdgeKind::References, "unresolved:Dir".to_string()),
                ("f", EdgeKind::References, "unresolved:Point".to_string()),
            ]
        );
    }

    /// The scan [`containers`] stands in for: the smallest range that
    /// strictly holds `child`'s, the first such index on a tie.
    fn innermost_container(ranges: &[Range<usize>], child: usize) -> Option<usize> {
        let held = &ranges[child];
        (0..ranges.len())
            .filter(|&s| {
                ranges[s].start <= held.start
                    && held.end <= ranges[s].end
                    && ranges[s].len() > held.len()
            })
            .min_by_key(|&s| ranges[s].len())
    }

    /// The scan [`enclosing`] stands in for: the smallest range that holds
    /// `offset`, the first such index on a tie.
    fn enclosing_def(ranges: &[Range<usize>], offset: usize) -> Option<usize> {
        (0..ranges.len())
            .filter(|&s| ranges[s].start <= offset && offset < ranges[s].end)
            .min_by_key(|&s| ranges[s].len())
    }

    /// A xorshift64 generator, so the cases are random and the same on every
    /// run.
    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    /// Split `span` into one to four slots and put one non-empty child range
    /// in each, after a random gap, then split each child the same way, down
    /// to `depth` levels.
    ///
    /// A child can fill its whole slot, so it can equal its parent's range.
    /// About one child in ten also gets a copy with the identical range.
    fn nested_ranges(
        rng: &mut XorShift,
        span: Range<usize>,
        depth: u32,
        out: &mut Vec<Range<usize>>,
    ) {
        let slots = 1 + rng.below(4);
        let slot = span.len() / slots;
        if depth == 0 || slot < 2 {
            return;
        }

        for n in 0..slots {
            let slot_start = span.start + n * slot;
            let start = slot_start + rng.below(slot / 2);
            let end = start + 1 + rng.below(slot_start + slot - start);
            out.push(start..end);
            if rng.below(10) == 0 {
                out.push(start..end);
            }
            nested_ranges(rng, start..end, depth - 1, out);
        }
    }

    #[test]
    fn sweeps_match_the_quadratic_scans() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..300 {
            let mut ranges = Vec::new();
            nested_ranges(&mut rng, 0..10_000, 4, &mut ranges);
            for i in (1..ranges.len()).rev() {
                ranges.swap(i, rng.below(i + 1));
            }
            let offsets: Vec<usize> = (0..200).map(|_| rng.below(10_100)).collect();

            let scanned: Vec<Option<usize>> = (0..ranges.len())
                .map(|child| innermost_container(&ranges, child))
                .collect();
            assert_eq!(containers(&ranges), scanned, "containers in {ranges:?}");

            let scanned: Vec<Option<usize>> = offsets
                .iter()
                .map(|&offset| enclosing_def(&ranges, offset))
                .collect();
            assert_eq!(
                enclosing(&ranges, &offsets),
                scanned,
                "enclosing {offsets:?} in {ranges:?}",
            );
        }
    }
}
