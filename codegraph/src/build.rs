//! Building a [`FileShard`] from one file's extracted definitions and
//! references.
//!
//! [`build_shard`] is pure: it turns the language crate's `SymbolDef`s and
//! `RefSite`s into the graph's [`Symbol`]s and [`Edge`]s, leaving call
//! targets unresolved for the merge step to link by name.

use crate::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
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

    let mut edges = contains_edges(&symbols);
    edges.extend(reference_edges(&symbols, &refs));

    FileShard {
        content_hash,
        symbols,
        edges,
    }
}

/// One [`EdgeKind::Contains`] edge per symbol that nests inside another,
/// from the innermost enclosing symbol to the nested one.
fn contains_edges(symbols: &[Symbol]) -> Vec<Edge> {
    symbols
        .iter()
        .filter_map(|child| {
            let parent = innermost_container(symbols, child)?;
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
/// The edge kind follows the reference kind. A call site becomes an
/// [`EdgeKind::Calls`] edge and a type-position use an [`EdgeKind::References`]
/// edge. The target carries the same [`RefKind`] so resolution stays
/// kind-aware.
fn reference_edges(symbols: &[Symbol], refs: &[RefSite]) -> Vec<Edge> {
    refs.iter()
        .filter_map(|r| {
            let caller = enclosing_def(symbols, r.site_range.start)?;
            Some(Edge {
                from: caller.key,
                to: Target::Unresolved {
                    name: r.name.clone(),
                    kind: r.kind,
                },
                kind: match r.kind {
                    RefKind::Call => EdgeKind::Calls,
                    RefKind::Type => EdgeKind::References,
                },
                site_range: r.site_range.clone(),
                confidence: Confidence::NameMatch,
            })
        })
        .collect()
}

/// The smallest symbol whose def-range strictly contains `child`'s, or
/// `None` for a top-level symbol.
fn innermost_container<'a>(symbols: &'a [Symbol], child: &Symbol) -> Option<&'a Symbol> {
    symbols
        .iter()
        .filter(|s| {
            s.def_range.start <= child.def_range.start
                && child.def_range.end <= s.def_range.end
                && range_len(s) > range_len(child)
        })
        .min_by_key(|s| range_len(s))
}

/// The smallest symbol whose def-range contains `offset`, or `None` when
/// the offset lies outside every definition.
fn enclosing_def(symbols: &[Symbol], offset: usize) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.def_range.start <= offset && offset < s.def_range.end)
        .min_by_key(|s| range_len(s))
}

fn range_len(symbol: &Symbol) -> usize {
    symbol.def_range.end - symbol.def_range.start
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
    use super::build_shard;
    use crate::{CodeGraph, Confidence, EdgeKind, FileId, FileShard, SymbolKey, Target};
    use std::collections::HashMap;
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
        let defs = extract_symbols(
            rust.outline_query.as_ref().unwrap(),
            tree.root_node(),
            &rope,
        );
        let refs = extract_references(rust.tags_query.as_ref().unwrap(), tree.root_node(), &rope);
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
                .entry((s.name.clone(), s.kind))
                .or_default()
                .push(s.key);
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
}
