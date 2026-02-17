use super::SyntaxIndex;
use std::ops::Range;
use stoat_text::Language;
use sum_tree::{Item, SumTree, Summary};
use text::{Anchor, BufferSnapshot, ToOffset};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Const,
    Static,
    TypeAlias,
    Module,
    Macro,
}

#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub range: Range<Anchor>,
    pub name_range: Range<Anchor>,
    pub kind: SymbolKind,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolSummary {
    pub range: Range<Anchor>,
    pub count: usize,
}

impl Item for SymbolEntry {
    type Summary = SymbolSummary;

    fn summary(&self, _cx: &BufferSnapshot) -> SymbolSummary {
        SymbolSummary {
            range: self.range.clone(),
            count: 1,
        }
    }
}

impl Summary for SymbolSummary {
    type Context<'a> = &'a BufferSnapshot;

    fn zero<'a>(_cx: Self::Context<'a>) -> Self {
        Self::default()
    }

    fn add_summary<'a>(&mut self, other: &Self, buffer: Self::Context<'a>) {
        if self.range == (Anchor::MAX..Anchor::MAX) {
            self.range = other.range.clone();
        } else if other.range != (Anchor::MAX..Anchor::MAX) {
            if other.range.start.cmp(&self.range.start, buffer).is_lt() {
                self.range.start = other.range.start;
            }
            if other.range.end.cmp(&self.range.end, buffer).is_gt() {
                self.range.end = other.range.end;
            }
        }
        self.count += other.count;
    }
}

#[derive(Clone)]
pub struct SymbolIndex {
    symbols: SumTree<SymbolEntry>,
    version: clock::Global,
}

impl SyntaxIndex for SymbolIndex {
    fn rebuild(
        tree: &stoat_text::tree_sitter::Tree,
        source: &str,
        buffer: &BufferSnapshot,
        language: Language,
    ) -> Self {
        let entries = extract_symbols(tree, source, buffer, language);
        let symbols = SumTree::from_iter(entries, buffer);
        Self {
            symbols,
            version: buffer.version().clone(),
        }
    }
}

impl SymbolIndex {
    pub fn snapshot(&self) -> SymbolSnapshot {
        SymbolSnapshot {
            symbols: self.symbols.clone(),
        }
    }
}

#[derive(Clone)]
pub struct SymbolSnapshot {
    symbols: SumTree<SymbolEntry>,
}

impl SymbolSnapshot {
    pub fn symbols_in_range(
        &self,
        range: Range<usize>,
        buffer: &BufferSnapshot,
    ) -> Vec<SymbolEntry> {
        let mut result = Vec::new();
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            let end = entry.range.end.to_offset(buffer);
            if start >= range.end {
                break;
            }
            if end > range.start {
                result.push(entry.clone());
            }
            cursor.next();
        }
        result
    }

    pub fn symbol_at_offset(&self, offset: usize, buffer: &BufferSnapshot) -> Option<SymbolEntry> {
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            let end = entry.range.end.to_offset(buffer);
            if start > offset {
                break;
            }
            if start <= offset && offset < end {
                return Some(entry.clone());
            }
            cursor.next();
        }
        None
    }

    pub fn next_symbol(&self, offset: usize, buffer: &BufferSnapshot) -> Option<SymbolEntry> {
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            if start > offset {
                return Some(entry.clone());
            }
            cursor.next();
        }
        None
    }

    pub fn prev_symbol(&self, offset: usize, buffer: &BufferSnapshot) -> Option<SymbolEntry> {
        let mut result = None;
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            if start >= offset {
                break;
            }
            let end = entry.range.end.to_offset(buffer);
            if offset >= end {
                result = Some(entry.clone());
            }
            cursor.next();
        }
        result
    }

    pub fn next_of_kind(
        &self,
        offset: usize,
        kind: SymbolKind,
        buffer: &BufferSnapshot,
    ) -> Option<SymbolEntry> {
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            if start > offset && entry.kind == kind {
                return Some(entry.clone());
            }
            cursor.next();
        }
        None
    }

    pub fn prev_of_kind(
        &self,
        offset: usize,
        kind: SymbolKind,
        buffer: &BufferSnapshot,
    ) -> Option<SymbolEntry> {
        let mut result = None;
        let mut cursor = self.symbols.cursor::<SymbolSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            if start >= offset {
                break;
            }
            let end = entry.range.end.to_offset(buffer);
            if offset >= end && entry.kind == kind {
                result = Some(entry.clone());
            }
            cursor.next();
        }
        result
    }
}

pub fn extract_symbols(
    tree: &stoat_text::tree_sitter::Tree,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
) -> Vec<SymbolEntry> {
    let mut entries = Vec::new();
    match language {
        Language::Rust => extract_rust_symbols(tree.root_node(), source, buffer, &mut entries),
        _ => {},
    }
    entries
}

fn extract_rust_symbols(
    node: stoat_text::tree_sitter::Node,
    source: &str,
    buffer: &BufferSnapshot,
    entries: &mut Vec<SymbolEntry>,
) {
    let kind = match node.kind() {
        "function_item" => Some(SymbolKind::Function),
        "struct_item" => Some(SymbolKind::Struct),
        "enum_item" => Some(SymbolKind::Enum),
        "trait_item" => Some(SymbolKind::Trait),
        "impl_item" => Some(SymbolKind::Impl),
        "const_item" => Some(SymbolKind::Const),
        "static_item" => Some(SymbolKind::Static),
        "type_item" => Some(SymbolKind::TypeAlias),
        "mod_item" => Some(SymbolKind::Module),
        "macro_definition" => Some(SymbolKind::Macro),
        _ => None,
    };

    if let Some(symbol_kind) = kind {
        let range = node.byte_range();
        let anchor_start = buffer.anchor_before(range.start);
        let anchor_end = buffer.anchor_after(range.end);

        let (name, name_range) = if let Some(name_node) = node.child_by_field_name("name") {
            let nr = name_node.byte_range();
            let name_str = &source[nr.clone()];
            let name_anchor_start = buffer.anchor_before(nr.start);
            let name_anchor_end = buffer.anchor_after(nr.end);
            (name_str.to_string(), name_anchor_start..name_anchor_end)
        } else if symbol_kind == SymbolKind::Impl {
            // impl blocks: use the type being implemented
            if let Some(type_node) = node.child_by_field_name("type") {
                let nr = type_node.byte_range();
                let name_str = &source[nr.clone()];
                let name_anchor_start = buffer.anchor_before(nr.start);
                let name_anchor_end = buffer.anchor_after(nr.end);
                (name_str.to_string(), name_anchor_start..name_anchor_end)
            } else {
                ("impl".to_string(), anchor_start..anchor_end)
            }
        } else {
            (String::new(), anchor_start..anchor_end)
        };

        entries.push(SymbolEntry {
            range: anchor_start..anchor_end,
            name_range,
            kind: symbol_kind,
            name,
        });
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            extract_rust_symbols(cursor.node(), source, buffer, entries);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Buffer;

    fn parse_and_index(source: &str) -> (BufferSnapshot, SymbolIndex) {
        let buffer = Buffer::new(0, text::BufferId::new(1).unwrap(), source);
        let snapshot = buffer.snapshot();
        let mut parser = stoat_text::Parser::new(Language::Rust).unwrap();
        let _ = parser.parse(source, &snapshot).unwrap();
        let tree = parser.tree().unwrap();
        let index = SymbolIndex::rebuild(tree, source, &snapshot, Language::Rust);
        (snapshot, index)
    }

    #[test]
    fn extracts_function() {
        let (buf, idx) = parse_and_index("fn hello() {}\nfn world() {}");
        let snap = idx.snapshot();
        let syms = snap.symbols_in_range(0..100, &buf);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "hello");
        assert_eq!(syms[0].kind, SymbolKind::Function);
        assert_eq!(syms[1].name, "world");
    }

    #[test]
    fn extracts_struct_and_enum() {
        let (buf, idx) = parse_and_index("struct Foo;\nenum Bar { A, B }");
        let snap = idx.snapshot();
        let syms = snap.symbols_in_range(0..100, &buf);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Foo");
        assert_eq!(syms[0].kind, SymbolKind::Struct);
        assert_eq!(syms[1].name, "Bar");
        assert_eq!(syms[1].kind, SymbolKind::Enum);
    }

    #[test]
    fn next_and_prev_symbol() {
        let (buf, idx) = parse_and_index("fn a() {}\nfn b() {}\nfn c() {}");
        let snap = idx.snapshot();

        let b_start = snap.next_symbol(0, &buf).unwrap();
        assert_eq!(b_start.name, "b");

        let a = snap.prev_symbol(15, &buf).unwrap();
        assert_eq!(a.name, "a");
    }

    #[test]
    fn symbol_at_offset() {
        let (buf, idx) = parse_and_index("fn hello() { let x = 1; }");
        let snap = idx.snapshot();
        let sym = snap.symbol_at_offset(5, &buf).unwrap();
        assert_eq!(sym.name, "hello");
    }

    #[test]
    fn extracts_impl() {
        let (buf, idx) = parse_and_index("struct Foo;\nimpl Foo { fn bar(&self) {} }");
        let snap = idx.snapshot();
        let syms = snap.symbols_in_range(0..100, &buf);
        assert!(syms
            .iter()
            .any(|s| s.kind == SymbolKind::Impl && s.name == "Foo"));
        assert!(syms
            .iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "bar"));
    }
}
