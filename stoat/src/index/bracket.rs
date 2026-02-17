use super::SyntaxIndex;
use std::ops::Range;
use stoat_text::Language;
use sum_tree::{Item, SumTree, Summary};
use text::{Anchor, BufferSnapshot, ToOffset};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketKind {
    Paren,
    Bracket,
    Brace,
    Angle,
}

#[derive(Debug, Clone)]
pub struct BracketEntry {
    pub open: Anchor,
    pub close: Anchor,
    pub kind: BracketKind,
}

#[derive(Debug, Clone, Default)]
pub struct BracketSummary {
    pub range: Range<Anchor>,
    pub count: usize,
}

impl Item for BracketEntry {
    type Summary = BracketSummary;

    fn summary(&self, _cx: &BufferSnapshot) -> BracketSummary {
        BracketSummary {
            range: self.open..self.close,
            count: 1,
        }
    }
}

impl Summary for BracketSummary {
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
pub struct BracketIndex {
    brackets: SumTree<BracketEntry>,
    version: clock::Global,
}

impl SyntaxIndex for BracketIndex {
    fn rebuild(
        tree: &stoat_text::tree_sitter::Tree,
        source: &str,
        buffer: &BufferSnapshot,
        language: Language,
    ) -> Self {
        let entries = extract_brackets(tree, source, buffer, language);
        let brackets = SumTree::from_iter(entries, buffer);
        Self {
            brackets,
            version: buffer.version().clone(),
        }
    }
}

impl BracketIndex {
    pub fn snapshot(&self) -> BracketSnapshot {
        BracketSnapshot {
            brackets: self.brackets.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BracketSnapshot {
    brackets: SumTree<BracketEntry>,
}

impl BracketSnapshot {
    /// Given a position near a bracket, return the matching bracket's position.
    pub fn matching_bracket(&self, offset: usize, buffer: &BufferSnapshot) -> Option<Anchor> {
        let mut cursor = self.brackets.cursor::<BracketSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let open = entry.open.to_offset(buffer);
            let close = entry.close.to_offset(buffer);
            if open > offset + 1 {
                break;
            }
            if offset == open || offset == open + 1 {
                return Some(entry.close);
            }
            if offset == close || (close > 0 && offset == close - 1) {
                return Some(entry.open);
            }
            cursor.next();
        }
        None
    }

    pub fn brackets_in_range(
        &self,
        range: Range<usize>,
        buffer: &BufferSnapshot,
    ) -> Vec<BracketEntry> {
        let mut result = Vec::new();
        let mut cursor = self.brackets.cursor::<BracketSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let open = entry.open.to_offset(buffer);
            let close = entry.close.to_offset(buffer);
            if open >= range.end {
                break;
            }
            if close > range.start {
                result.push(entry.clone());
            }
            cursor.next();
        }
        result
    }

    /// Find the innermost bracket pair containing offset.
    pub fn innermost_bracket_pair(
        &self,
        offset: usize,
        buffer: &BufferSnapshot,
    ) -> Option<BracketEntry> {
        let mut best: Option<BracketEntry> = None;
        let mut cursor = self.brackets.cursor::<BracketSummary>(buffer);
        cursor.next();
        while let Some(entry) = cursor.item() {
            let open = entry.open.to_offset(buffer);
            let close = entry.close.to_offset(buffer);
            if open > offset {
                break;
            }
            if open <= offset && offset <= close {
                if best.as_ref().map_or(true, |b| {
                    let b_open = b.open.to_offset(buffer);
                    let b_close = b.close.to_offset(buffer);
                    (close - open) < (b_close - b_open)
                }) {
                    best = Some(entry.clone());
                }
            }
            cursor.next();
        }
        best
    }
}

pub fn extract_brackets(
    tree: &stoat_text::tree_sitter::Tree,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
) -> Vec<BracketEntry> {
    let mut entries = Vec::new();
    match language {
        Language::Rust | Language::Json | Language::Toml => {
            extract_bracket_pairs(tree.root_node(), source, buffer, &mut entries);
        },
        _ => {},
    }
    entries
}

fn extract_bracket_pairs(
    node: stoat_text::tree_sitter::Node,
    source: &str,
    buffer: &BufferSnapshot,
    entries: &mut Vec<BracketEntry>,
) {
    let pairs: &[(&str, &str, BracketKind)] = &[
        ("(", ")", BracketKind::Paren),
        ("[", "]", BracketKind::Bracket),
        ("{", "}", BracketKind::Brace),
        ("<", ">", BracketKind::Angle),
    ];

    for &(open_str, close_str, kind) in pairs {
        let mut open_node = None;
        let mut close_node = None;

        let child_count = node.child_count();
        for i in 0..child_count {
            if let Some(child) = node.child(i) {
                let text = &source[child.byte_range()];
                if text == open_str && open_node.is_none() {
                    open_node = Some(child);
                } else if text == close_str && open_node.is_some() {
                    close_node = Some(child);
                    break;
                }
            }
        }

        if let (Some(open), Some(close)) = (open_node, close_node) {
            entries.push(BracketEntry {
                open: buffer.anchor_before(open.start_byte()),
                close: buffer.anchor_before(close.start_byte()),
                kind,
            });
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            extract_bracket_pairs(cursor.node(), source, buffer, entries);
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

    fn parse_and_index(source: &str) -> (BufferSnapshot, BracketIndex) {
        let buffer = Buffer::new(0, text::BufferId::new(1).unwrap(), source);
        let snapshot = buffer.snapshot();
        let mut parser = stoat_text::Parser::new(Language::Rust).unwrap();
        let _ = parser.parse(source, &snapshot).unwrap();
        let tree = parser.tree().unwrap();
        let index = BracketIndex::rebuild(tree, source, &snapshot, Language::Rust);
        (snapshot, index)
    }

    #[test]
    fn extracts_parens_and_braces() {
        let (buf, idx) = parse_and_index("fn hello() { let x = (1, 2); }");
        let snap = idx.snapshot();
        let brackets = snap.brackets_in_range(0..100, &buf);
        assert!(brackets.iter().any(|b| b.kind == BracketKind::Paren));
        assert!(brackets.iter().any(|b| b.kind == BracketKind::Brace));
    }

    #[test]
    fn matching_bracket() {
        let source = "fn hello() {}";
        let (buf, idx) = parse_and_index(source);
        let snap = idx.snapshot();

        let open_paren_offset = source.find('(').unwrap();
        let close_paren_offset = source.find(')').unwrap();

        let matched = snap.matching_bracket(open_paren_offset, &buf);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().to_offset(&buf), close_paren_offset);
    }

    #[test]
    fn innermost_bracket_pair() {
        let source = "fn a() { let x = (1 + 2); }";
        let (buf, idx) = parse_and_index(source);
        let snap = idx.snapshot();

        let inner = snap.innermost_bracket_pair(19, &buf).unwrap();
        assert_eq!(inner.kind, BracketKind::Paren);
    }
}
