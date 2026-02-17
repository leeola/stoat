use super::SyntaxIndex;
use std::ops::Range;
use stoat_text::Language;
use sum_tree::{Item, SumTree, Summary};
use text::{Anchor, BufferSnapshot, ToOffset};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Function,
    Block,
    If,
    Else,
    Match,
    MatchArm,
    For,
    While,
    Loop,
    Closure,
    Impl,
    Trait,
}

#[derive(Debug, Clone)]
pub struct ScopeEntry {
    pub range: Range<Anchor>,
    pub body_range: Range<Anchor>,
    pub kind: ScopeKind,
    pub depth: u16,
    pub parent: Option<Anchor>,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeSummary {
    pub range: Range<Anchor>,
    pub count: usize,
    pub max_depth: u16,
}

impl Item for ScopeEntry {
    type Summary = ScopeSummary;

    fn summary(&self, _cx: &BufferSnapshot) -> ScopeSummary {
        ScopeSummary {
            range: self.range.clone(),
            count: 1,
            max_depth: self.depth,
        }
    }
}

impl Summary for ScopeSummary {
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
        self.max_depth = self.max_depth.max(other.max_depth);
    }
}

#[derive(Clone)]
pub struct ScopeIndex {
    scopes: SumTree<ScopeEntry>,
}

impl SyntaxIndex for ScopeIndex {
    fn rebuild(
        tree: &stoat_text::tree_sitter::Tree,
        source: &str,
        buffer: &BufferSnapshot,
        language: Language,
    ) -> Self {
        let entries = extract_scopes(tree, source, buffer, language);
        let scopes = SumTree::from_iter(entries, buffer);
        Self { scopes }
    }
}

impl ScopeIndex {
    pub fn snapshot(&self) -> ScopeSnapshot {
        ScopeSnapshot {
            scopes: self.scopes.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ScopeSnapshot {
    scopes: SumTree<ScopeEntry>,
}

impl ScopeSnapshot {
    /// Find the innermost scope containing offset.
    pub fn scope_at_offset(&self, offset: usize, buffer: &BufferSnapshot) -> Option<ScopeEntry> {
        let mut best: Option<ScopeEntry> = None;
        let mut cursor = self.scopes.cursor::<ScopeSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let start = entry.range.start.to_offset(buffer);
            let end = entry.range.end.to_offset(buffer);
            if start > offset {
                break;
            }
            if start <= offset && offset < end {
                if best.as_ref().map_or(true, |b| entry.depth > b.depth) {
                    best = Some(entry.clone());
                }
            }
            cursor.next();
        }
        best
    }

    pub fn parent_scope(&self, entry: &ScopeEntry, buffer: &BufferSnapshot) -> Option<ScopeEntry> {
        let parent_anchor = entry.parent.as_ref()?;
        let parent_offset = parent_anchor.to_offset(buffer);
        let mut cursor = self.scopes.cursor::<ScopeSummary>(buffer);
        cursor.next();
        while let Some(e) = cursor.item() {
            let start = e.range.start.to_offset(buffer);
            if start == parent_offset {
                return Some(e.clone());
            }
            if start > parent_offset {
                break;
            }
            cursor.next();
        }
        None
    }

    pub fn scopes_in_range(&self, range: Range<usize>, buffer: &BufferSnapshot) -> Vec<ScopeEntry> {
        let mut result = Vec::new();
        let mut cursor = self.scopes.cursor::<ScopeSummary>(buffer);
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

    pub fn scopes_at_depth(
        &self,
        depth: u16,
        range: Range<usize>,
        buffer: &BufferSnapshot,
    ) -> Vec<ScopeEntry> {
        self.scopes_in_range(range, buffer)
            .into_iter()
            .filter(|e| e.depth == depth)
            .collect()
    }
}

pub fn extract_scopes(
    tree: &stoat_text::tree_sitter::Tree,
    _source: &str,
    buffer: &BufferSnapshot,
    language: Language,
) -> Vec<ScopeEntry> {
    let mut entries = Vec::new();
    match language {
        Language::Rust => {
            extract_rust_scopes(tree.root_node(), buffer, &mut entries, 0, None);
        },
        _ => {},
    }
    entries
}

fn extract_rust_scopes(
    node: stoat_text::tree_sitter::Node,
    buffer: &BufferSnapshot,
    entries: &mut Vec<ScopeEntry>,
    depth: u16,
    parent: Option<Anchor>,
) {
    let scope_kind = match node.kind() {
        "function_item" => Some(ScopeKind::Function),
        "block" => Some(ScopeKind::Block),
        "if_expression" => Some(ScopeKind::If),
        "else_clause" => Some(ScopeKind::Else),
        "match_expression" => Some(ScopeKind::Match),
        "match_arm" => Some(ScopeKind::MatchArm),
        "for_expression" => Some(ScopeKind::For),
        "while_expression" => Some(ScopeKind::While),
        "loop_expression" => Some(ScopeKind::Loop),
        "closure_expression" => Some(ScopeKind::Closure),
        "impl_item" => Some(ScopeKind::Impl),
        "trait_item" => Some(ScopeKind::Trait),
        _ => None,
    };

    let new_parent = if let Some(kind) = scope_kind {
        let range = node.byte_range();
        let anchor_start = buffer.anchor_before(range.start);
        let anchor_end = buffer.anchor_after(range.end);

        let body_range = if let Some(body) = node.child_by_field_name("body") {
            let br = body.byte_range();
            buffer.anchor_before(br.start)..buffer.anchor_after(br.end)
        } else {
            anchor_start..anchor_end
        };

        entries.push(ScopeEntry {
            range: anchor_start..anchor_end,
            body_range,
            kind,
            depth,
            parent,
        });

        Some(anchor_start)
    } else {
        parent
    };

    let new_depth = if scope_kind.is_some() {
        depth + 1
    } else {
        depth
    };

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            extract_rust_scopes(cursor.node(), buffer, entries, new_depth, new_parent);
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

    fn parse_and_index(source: &str) -> (BufferSnapshot, ScopeIndex) {
        let buffer = Buffer::new(0, text::BufferId::new(1).unwrap(), source);
        let snapshot = buffer.snapshot();
        let mut parser = stoat_text::Parser::new(Language::Rust).unwrap();
        parser.parse(source).unwrap();
        let tree = parser.tree().unwrap();
        let index = ScopeIndex::rebuild(tree, source, &snapshot, Language::Rust);
        (snapshot, index)
    }

    #[test]
    fn extracts_function_scope() {
        let (buf, idx) = parse_and_index("fn hello() { let x = 1; }");
        let snap = idx.snapshot();
        let scopes = snap.scopes_in_range(0..100, &buf);
        assert!(scopes.iter().any(|s| s.kind == ScopeKind::Function));
    }

    #[test]
    fn extracts_nested_scopes() {
        let (buf, idx) = parse_and_index("fn hello() { if true { let x = 1; } }");
        let snap = idx.snapshot();
        let scopes = snap.scopes_in_range(0..100, &buf);
        assert!(scopes.iter().any(|s| s.kind == ScopeKind::Function));
        assert!(scopes.iter().any(|s| s.kind == ScopeKind::If));
    }

    #[test]
    fn scope_at_offset_finds_innermost() {
        let source = "fn hello() { if true { let x = 1; } }";
        let (buf, idx) = parse_and_index(source);
        let snap = idx.snapshot();
        let inner = snap.scope_at_offset(25, &buf).unwrap();
        assert!(inner.depth > 0);
    }

    #[test]
    fn scopes_at_depth() {
        let (buf, idx) = parse_and_index("fn a() { if true {} }\nfn b() { if false {} }");
        let snap = idx.snapshot();
        let depth0 = snap.scopes_at_depth(0, 0..100, &buf);
        assert_eq!(depth0.len(), 2);
        assert!(depth0.iter().all(|s| s.kind == ScopeKind::Function));
    }
}
