//! Definition extraction from a parsed buffer.
//!
//! [`extract_symbols`] runs a language's `outline.scm` query over a
//! syntax tree and turns each `@item`/`@name` match into a [`SymbolDef`]:
//! the definitions a file declares, with their byte ranges and an
//! enclosing-container path. It is the definition half of the code graph.
//! Call-site references are collected separately.
//!
//! Pure tree-sitter logic only -- no IO -- so it stays unit-testable over
//! fixed snippets.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use stoat_text::Rope;
use tree_sitter::{Node, Query, StreamingIterator};

/// A single named definition the source declares, such as a function,
/// type, or field.
///
/// `def_range` spans the whole definition (the `@item` capture), while
/// `name_range` spans just the identifier (the `@name` capture) and
/// always falls inside `def_range`. `container` is the enclosing
/// module/impl/struct/trait names, outermost first -- best-effort
/// metadata for display and disambiguation, not used for name resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymbolKind,
    pub def_range: Range<usize>,
    pub name_range: Range<usize>,
    pub container: Vec<String>,
}

/// The category of a [`SymbolDef`], derived from the `@item` node kind.
///
/// A function is [`SymbolKind::Method`] when enclosed by an impl or trait
/// and [`SymbolKind::Function`] otherwise. The grammar has no distinct
/// method node, so the container decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Static,
    TypeAlias,
    Macro,
    Field,
    EnumVariant,
}

/// One use of a name the source does not define here, such as a call or a
/// type-position identifier.
///
/// `site_range` spans the referenced identifier (the `@reference.*`
/// capture) and `name` is its text. [`Self::kind`] records which kind of
/// reference it is.
///
/// This pass does not attribute the reference to its enclosing definition;
/// that attribution is resolved downstream by range containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSite {
    pub name: String,
    pub kind: RefKind,
    pub site_range: Range<usize>,
}

/// The kind of reference a [`RefSite`] records.
///
/// [`RefKind::Call`] is a function, method, or macro invocation;
/// [`RefKind::Type`] is a type-position identifier use, such as a type name
/// in a signature or field. Value-position identifier uses are not collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    Call,
    Type,
}

/// Extract every definition captured by `query` (an `outline.scm`) over
/// the tree rooted at `root`, reading identifier text from `rope`.
///
/// Returns an empty vector when `query` lacks an `@item` or `@name`
/// capture (i.e. it is not an outline query). Matches whose `@item` node
/// kind maps to no [`SymbolKind`] are skipped, so running a non-rust
/// outline query yields no symbols.
pub fn extract_symbols(query: &Query, root: Node<'_>, rope: &Rope) -> Vec<SymbolDef> {
    let mut out = Vec::new();
    let (Some(item_idx), Some(name_idx)) = (
        query.capture_index_for_name("item"),
        query.capture_index_for_name("name"),
    ) else {
        return out;
    };

    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    let mut matches = cursor_h.matches(query, root, provider);
    while let Some(m) = matches.next() {
        let mut item_node = None;
        let mut name_node = None;
        for cap in m.captures {
            if cap.index == item_idx {
                item_node = Some(cap.node);
            } else if cap.index == name_idx {
                name_node = Some(cap.node);
            }
        }

        let (Some(item), Some(name)) = (item_node, name_node) else {
            continue;
        };
        let Some(kind) = symbol_kind(item) else {
            continue;
        };

        out.push(SymbolDef {
            name: text_of(name, rope),
            kind,
            def_range: item.byte_range(),
            name_range: name.byte_range(),
            container: container_path(item, rope),
        });
    }
    out
}

/// Collect every reference site captured by `query` (a `tags.scm`) over the
/// tree rooted at `root`, reading identifier text from `rope`.
///
/// Each `@reference.*` capture becomes one [`RefSite`] tagged with the
/// [`RefKind`] its capture name maps to. Returns an empty vector when `query`
/// has no `@reference.*` capture. The result is in the query cursor's match
/// order, not sorted.
pub fn extract_references(query: &Query, root: Node<'_>, rope: &Rope) -> Vec<RefSite> {
    let mut out = Vec::new();

    let kinds: Vec<(u32, RefKind)> = query
        .capture_names()
        .iter()
        .enumerate()
        .filter_map(|(idx, &name)| Some((idx as u32, ref_kind(name)?)))
        .collect();
    if kinds.is_empty() {
        return out;
    }

    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    let mut matches = cursor_h.matches(query, root, provider);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if let Some(&(_, kind)) = kinds.iter().find(|&&(idx, _)| idx == cap.index) {
                out.push(RefSite {
                    name: text_of(cap.node, rope),
                    kind,
                    site_range: cap.node.byte_range(),
                });
            }
        }
    }
    out
}

/// Map a tags-query capture name to the [`RefKind`] it records, or `None`
/// for any capture that is not a `@reference.*`.
fn ref_kind(capture_name: &str) -> Option<RefKind> {
    match capture_name {
        "reference.call" => Some(RefKind::Call),
        "reference.type" => Some(RefKind::Type),
        _ => None,
    }
}

fn symbol_kind(item: Node<'_>) -> Option<SymbolKind> {
    Some(match item.kind() {
        "function_item" | "function_signature_item" => function_kind(item),
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "enum_variant" => SymbolKind::EnumVariant,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "mod_item" => SymbolKind::Module,
        "type_item" | "associated_type" => SymbolKind::TypeAlias,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "macro_definition" => SymbolKind::Macro,
        "field_declaration" => SymbolKind::Field,
        _ => return None,
    })
}

fn function_kind(item: Node<'_>) -> SymbolKind {
    let mut ancestor = item.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "impl_item" | "trait_item") {
            return SymbolKind::Method;
        }
        ancestor = node.parent();
    }
    SymbolKind::Function
}

fn container_path(item: Node<'_>, rope: &Rope) -> Vec<String> {
    let mut path = Vec::new();
    let mut ancestor = item.parent();
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "mod_item" | "impl_item" | "struct_item" | "trait_item"
        ) && let Some(name) = node_name(node, rope)
        {
            path.push(name);
        }
        ancestor = node.parent();
    }
    path.reverse();
    path
}

/// The declared name of a definition node, read from its `name:` field,
/// or the `type:` field for an impl block (which has no name).
fn node_name(node: Node<'_>, rope: &Rope) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))?;
    Some(text_of(name, rope))
}

fn text_of(node: Node<'_>, rope: &Rope) -> String {
    rope.chunks_in_range(node.byte_range()).collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_references, extract_symbols, RefKind, SymbolDef, SymbolKind};
    use crate::{language::LanguageRegistry, parse_rope};
    use stoat_text::Rope;

    const SNIPPET: &str = "\
const MAX: u32 = 10;
static NAME: &str = \"s\";
type Coord = i32;

struct Point {
    x: i32,
}

enum Dir {
    North,
}

trait Greet {
    fn hello(&self);
}

impl Point {
    fn origin() -> Point {
        Point { x: 0 }
    }
}

mod inner {
    fn helper() {}
}

macro_rules! mac {
    () => {};
}

fn main() {
    helper();
}
";

    fn extract_sorted() -> Vec<SymbolDef> {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let query = rust.outline_query.as_ref().unwrap();
        let rope = Rope::from(SNIPPET);
        let tree = parse_rope(rust, &rope, None).unwrap();
        let mut syms = extract_symbols(query, tree.root_node(), &rope);
        syms.sort_by_key(|s| (s.def_range.start, s.def_range.end));
        syms
    }

    #[test]
    fn extract_symbols_over_rust_snippet() {
        let syms = extract_sorted();

        let semantic: Vec<(&str, SymbolKind, Vec<&str>)> = syms
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
            semantic,
            vec![
                ("MAX", SymbolKind::Const, vec![]),
                ("NAME", SymbolKind::Static, vec![]),
                ("Coord", SymbolKind::TypeAlias, vec![]),
                ("Point", SymbolKind::Struct, vec![]),
                ("x", SymbolKind::Field, vec!["Point"]),
                ("Dir", SymbolKind::Enum, vec![]),
                ("North", SymbolKind::EnumVariant, vec![]),
                ("Greet", SymbolKind::Trait, vec![]),
                ("hello", SymbolKind::Method, vec!["Greet"]),
                ("Point", SymbolKind::Impl, vec![]),
                ("origin", SymbolKind::Method, vec!["Point"]),
                ("inner", SymbolKind::Module, vec![]),
                ("helper", SymbolKind::Function, vec!["inner"]),
                ("mac", SymbolKind::Macro, vec![]),
                ("main", SymbolKind::Function, vec![]),
            ]
        );

        for s in &syms {
            assert_eq!(
                &SNIPPET[s.name_range.clone()],
                s.name,
                "name_range must slice to the name of {}",
                s.name
            );
            assert!(
                s.def_range.start <= s.name_range.start && s.name_range.end <= s.def_range.end,
                "def_range must contain name_range for {}",
                s.name
            );
        }

        let def = |name: &str| {
            let s = syms.iter().find(|s| s.name == name).unwrap();
            &SNIPPET[s.def_range.clone()]
        };
        assert_eq!(def("MAX"), "const MAX: u32 = 10;");
        assert_eq!(def("helper"), "fn helper() {}");
        assert_eq!(def("hello"), "fn hello(&self);");
    }

    const CALLS: &str = "\
fn demo() {
    free();
    obj.method();
    helper::scoped();
    println!(\"x\");
    vec![1, 2];
}
";

    #[test]
    fn extract_references_over_rust_snippet() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let query = rust.tags_query.as_ref().unwrap();
        let rope = Rope::from(CALLS);
        let tree = parse_rope(rust, &rope, None).unwrap();
        let mut refs = extract_references(query, tree.root_node(), &rope);
        refs.sort_by_key(|r| r.site_range.start);

        let got: Vec<(&str, RefKind)> = refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("free", RefKind::Call),
                ("method", RefKind::Call),
                ("scoped", RefKind::Call),
                ("println", RefKind::Call),
                ("vec", RefKind::Call),
            ]
        );

        for r in &refs {
            assert_eq!(
                &CALLS[r.site_range.clone()],
                r.name,
                "site_range must slice to the call name {}",
                r.name
            );
        }
    }

    const TYPE_USES: &str = "fn f(p: Point) -> Dir {}";

    #[test]
    fn extract_type_references_over_rust_snippet() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let query = rust.tags_query.as_ref().unwrap();
        let rope = Rope::from(TYPE_USES);
        let tree = parse_rope(rust, &rope, None).unwrap();
        let mut refs = extract_references(query, tree.root_node(), &rope);
        refs.sort_by_key(|r| r.site_range.start);

        let got: Vec<(&str, RefKind)> = refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(got, vec![("Point", RefKind::Type), ("Dir", RefKind::Type)]);

        for r in &refs {
            assert_eq!(
                &TYPE_USES[r.site_range.clone()],
                r.name,
                "site_range must slice to the type name {}",
                r.name
            );
        }
    }

    #[test]
    fn scoped_type_yields_single_reference() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        let query = rust.tags_query.as_ref().unwrap();
        let rope = Rope::from("fn f(p: foo::Bar) {}");
        let tree = parse_rope(rust, &rope, None).unwrap();
        let refs = extract_references(query, tree.root_node(), &rope);

        let got: Vec<(&str, RefKind)> = refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(got, vec![("Bar", RefKind::Type)]);
    }
}
