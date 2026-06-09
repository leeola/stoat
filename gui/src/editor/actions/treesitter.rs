use crate::editor::{actions::movement::extend_head, Editor, EditorEvent};
use gpui::Context;
use std::ops::Range;
use stoat_language::{collect_capture_starts, SyntaxLayer, SyntaxMap};
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// Selected parent-node bound for [`Editor::handle_move_parent_bound`].
/// `Start` lands the cursor on the parent's first byte; `End` lands on
/// the parent's last byte.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeBound {
    Start,
    End,
}

/// Sibling-walk direction for [`Editor::handle_select_sibling`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SiblingDir {
    Next,
    Prev,
}

/// Textobject family for [`Editor::handle_goto_textobject`]. Maps to
/// the `function.around` / `class.around` capture name in the active
/// layer's textobjects query.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NavKind {
    Function,
    Class,
}

impl NavKind {
    fn capture_name(self) -> &'static str {
        match self {
            NavKind::Function => "function.around",
            NavKind::Class => "class.around",
        }
    }
}

/// Direction passed to [`Editor::handle_goto_textobject`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NavDirection {
    Next,
    Prev,
}

impl Editor {
    /// Expand the primary selection to the smallest enclosing
    /// tree-sitter node `count` times. Each step pushes the current
    /// range onto the expansion history so [`Self::handle_shrink_selection`]
    /// can unwind. When the current range already matches a node
    /// exactly, expansion walks to that node's parent.
    pub fn handle_expand_selection(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let mut effective = 0;
        for _ in 0..count {
            if !self.expand_selection_step(cx) {
                break;
            }
            effective += 1;
        }
        if effective > 0 {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    fn expand_selection_step(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = newest_selection(&self.selections, &snapshot);
        let sel_start = snapshot.resolve_anchor(&sel.start);
        let sel_end = snapshot.resolve_anchor(&sel.end);

        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return false;
        };

        let target = singleton.read(cx).syntax_map().and_then(|map| {
            let layer = deepest_containing_layer(map, sel_start, sel_end)?;
            let root = layer.tree.root_node();
            let node = root.descendant_for_byte_range(sel_start, sel_end)?;
            let node_range = node.byte_range();
            if node_range.start == sel_start && node_range.end == sel_end {
                node.parent().map(|p| p.byte_range())
            } else {
                Some(node_range)
            }
        });
        let Some(target) = target else {
            return false;
        };

        let current_range = sel_start..sel_end;
        if self.expansion_tip.as_ref() != Some(&current_range) {
            self.expansion_history.clear();
        }
        self.expansion_history.push(current_range);
        self.expansion_tip = Some(target.clone());
        apply_primary_range(&mut self.selections, &snapshot, target);
        true
    }

    /// Step the primary selection back to a previously-stored range
    /// from [`Self::handle_expand_selection`]. No-op when the history
    /// is empty. Each call consumes `count` history entries.
    pub fn handle_shrink_selection(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let mut target: Option<Range<usize>> = None;
        for _ in 0..count {
            match self.expansion_history.pop() {
                Some(t) => target = Some(t),
                None => break,
            }
        }
        let Some(target) = target else {
            return;
        };
        self.expansion_tip = Some(target.clone());
        apply_primary_range(&mut self.selections, &snapshot, target);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Walk the primary selection's enclosing node to its next or
    /// previous named sibling `count` times. With `extend = false`
    /// the selection collapses to the sibling's full range; with
    /// `extend = true` only the head moves to the sibling boundary
    /// (next-sibling end / prev-sibling start) so the tail is
    /// preserved.
    pub fn handle_select_sibling(
        &mut self,
        dir: SiblingDir,
        extend: bool,
        count: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = newest_selection(&self.selections, &snapshot);
        let sel_start = snapshot.resolve_anchor(&sel.start);
        let sel_end = snapshot.resolve_anchor(&sel.end);

        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let target = singleton.read(cx).syntax_map().and_then(|map| {
            let layer = deepest_containing_layer(map, sel_start, sel_end)?;
            let root = layer.tree.root_node();
            let node = root.descendant_for_byte_range(sel_start, sel_end)?;
            let mut current = node;
            let mut moved = false;
            for _ in 0..count {
                let next = match dir {
                    SiblingDir::Next => current.next_named_sibling(),
                    SiblingDir::Prev => current.prev_named_sibling(),
                };
                match next {
                    Some(s) => {
                        current = s;
                        moved = true;
                    },
                    None => break,
                }
            }
            if moved {
                Some(current.byte_range())
            } else {
                None
            }
        });
        let Some(target) = target else {
            return;
        };

        if extend {
            let head_offset = match dir {
                SiblingDir::Next => target.end,
                SiblingDir::Prev => target.start,
            };
            let head_anchor = snapshot.anchor_at(head_offset, Bias::Right);
            let new_disjoint: Vec<Selection<Anchor>> = self
                .selections
                .all_anchors()
                .iter()
                .map(|sel| extend_head(sel, head_anchor, head_offset, sel.goal, &snapshot))
                .collect();
            self.selections.replace_with(new_disjoint, &snapshot);
        } else {
            apply_primary_range(&mut self.selections, &snapshot, target);
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Split every selection across the named children of the smallest
    /// enclosing parent that has more than one child. Mirrors the
    /// Helix `*` keybind: from a cursor inside one child, expand to
    /// cover every sibling of that child.
    pub fn handle_select_all_siblings(&mut self, cx: &mut Context<'_, Self>) {
        self.fan_selections_to_children(true, cx);
    }

    /// Split every selection across the named children of the
    /// smallest enclosing node, preserving the current depth. Mirrors
    /// the Helix `S-Alt-,` keybind.
    pub fn handle_select_all_children(&mut self, cx: &mut Context<'_, Self>) {
        self.fan_selections_to_children(false, cx);
    }

    fn fan_selections_to_children(
        &mut self,
        walk_to_multichild_parent: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };

        let buffer_ref = singleton.read(cx);
        let Some(syntax_map) = buffer_ref.syntax_map() else {
            return;
        };

        let mut next_id = self
            .selections
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let mut new_disjoint: Vec<Selection<Anchor>> = Vec::new();
        for sel in self.selections.all_anchors() {
            let sel_start = snapshot.resolve_anchor(&sel.start);
            let sel_end = snapshot.resolve_anchor(&sel.end);
            let Some(layer) = deepest_containing_layer(syntax_map, sel_start, sel_end) else {
                new_disjoint.push(sel.clone());
                continue;
            };
            let root = layer.tree.root_node();
            let Some(node) = root.descendant_for_byte_range(sel_start, sel_end) else {
                new_disjoint.push(sel.clone());
                continue;
            };
            let parent_node = if walk_to_multichild_parent {
                let mut current = node.parent();
                while let Some(p) = current {
                    if p.named_child_count() > 1 {
                        break;
                    }
                    current = p.parent();
                }
                current
            } else {
                Some(node)
            };
            let Some(parent_node) = parent_node else {
                new_disjoint.push(sel.clone());
                continue;
            };
            let mut walker = parent_node.walk();
            let mut produced = false;
            for child in parent_node.named_children(&mut walker) {
                let range = child.byte_range();
                let start_anchor = snapshot.anchor_at(range.start, Bias::Right);
                let end_anchor = snapshot.anchor_at(range.end, Bias::Right);
                new_disjoint.push(Selection {
                    id: next_id,
                    start: start_anchor,
                    end: end_anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                });
                next_id += 1;
                produced = true;
            }
            if !produced {
                new_disjoint.push(sel.clone());
            }
        }
        if new_disjoint.is_empty() {
            return;
        }
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Walk `count` parents up from the smallest node containing the
    /// primary selection, then land the cursor on that parent's
    /// `bound` byte. With `extend = true` only the head moves so the
    /// tail is preserved.
    pub fn handle_move_parent_bound(
        &mut self,
        bound: NodeBound,
        extend: bool,
        count: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = newest_selection(&self.selections, &snapshot);
        let sel_start = snapshot.resolve_anchor(&sel.start);
        let sel_end = snapshot.resolve_anchor(&sel.end);

        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let target_offset = singleton.read(cx).syntax_map().and_then(|map| {
            let layer = deepest_containing_layer(map, sel_start, sel_end)?;
            let root = layer.tree.root_node();
            let node = root.descendant_for_byte_range(sel_start, sel_end)?;
            let mut current = node;
            let mut moved = false;
            for _ in 0..count {
                match current.parent() {
                    Some(p) => {
                        current = p;
                        moved = true;
                    },
                    None => break,
                }
            }
            if !moved {
                return None;
            }
            Some(match bound {
                NodeBound::Start => current.start_byte(),
                NodeBound::End => current.end_byte(),
            })
        });
        let Some(target_offset) = target_offset else {
            return;
        };

        if extend {
            let head_anchor = snapshot.anchor_at(target_offset, Bias::Right);
            let new_disjoint: Vec<Selection<Anchor>> = self
                .selections
                .all_anchors()
                .iter()
                .map(|sel| extend_head(sel, head_anchor, target_offset, sel.goal, &snapshot))
                .collect();
            self.selections.replace_with(new_disjoint, &snapshot);
        } else {
            apply_primary_range(
                &mut self.selections,
                &snapshot,
                target_offset..target_offset,
            );
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump the primary selection to the bracket that matches the
    /// one under the cursor's head, collapsing every selection to
    /// that target. No-op when the cursor is not on a bracket
    /// character (`()[]{}`), the cursor is inside a string or
    /// comment node (when a syntax tree is available), no match
    /// exists in the requested direction, or the buffer is
    /// multi-excerpt. Bracket characters inside string/comment
    /// nodes are skipped during the scan when the syntax tree is
    /// available.
    pub fn handle_match_brackets(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = newest_selection(&self.selections, &snapshot);
        let head = snapshot.resolve_anchor(&sel.head());

        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };

        let target = {
            let buffer = singleton.read(cx);
            let rope = buffer.read(|b| b.rope().clone());
            let tree = buffer
                .syntax_map()
                .and_then(|map| deepest_containing_layer(map, head, head).map(|layer| &layer.tree));
            stoat_language::bracket::match_bracket_target(&rope, head, tree)
        };
        let Some(target) = target else {
            return;
        };

        let target_anchor = snapshot.anchor_at(target, Bias::Right);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let mut new = sel.clone();
                new.collapse_to(target_anchor, SelectionGoal::None);
                new
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump the primary selection to the next or previous textobject
    /// of `kind` (function or class). No-op when the buffer's language
    /// has no `textobjects.scm` query or no match exists in the
    /// requested direction.
    pub fn handle_goto_textobject(
        &mut self,
        kind: NavKind,
        direction: NavDirection,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = newest_selection(&self.selections, &snapshot);
        let cursor = snapshot.resolve_anchor(&sel.head());

        let Some(singleton) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let starts = {
            let buffer = singleton.read(cx);
            let Some(syntax_map) = buffer.syntax_map() else {
                return;
            };
            let Some(layer) = deepest_containing_layer(syntax_map, cursor, cursor) else {
                return;
            };
            let Some(query) = layer.language.textobjects_query.as_ref() else {
                return;
            };
            buffer.read(|b| {
                collect_capture_starts(query, layer.tree.root_node(), b.rope(), kind.capture_name())
            })
        };
        let target = match direction {
            NavDirection::Next => starts.into_iter().find(|&s| s > cursor),
            NavDirection::Prev => starts.into_iter().rev().find(|&s| s < cursor),
        };
        let Some(target) = target else {
            return;
        };

        let target_anchor = snapshot.anchor_at(target, Bias::Right);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let mut new = sel.clone();
                new.collapse_to(target_anchor, SelectionGoal::None);
                new
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

fn newest_selection<'a>(
    selections: &'a stoat::selection::SelectionsCollection,
    _snapshot: &stoat::multi_buffer::MultiBufferSnapshot,
) -> &'a Selection<Anchor> {
    selections
        .all_anchors()
        .iter()
        .max_by_key(|s| s.id)
        .expect("SelectionsCollection invariant: at least one selection")
}

fn deepest_containing_layer(
    map: &SyntaxMap,
    sel_start: usize,
    sel_end: usize,
) -> Option<&SyntaxLayer> {
    map.snapshot().iter_layers().fold(None, |acc, layer| {
        let start = layer.start_offset as usize;
        let end = layer.end_offset as usize;
        if start <= sel_start && end >= sel_end {
            match acc {
                Some(prev) if prev.depth >= layer.depth => acc,
                _ => Some(layer),
            }
        } else {
            acc
        }
    })
}

fn apply_primary_range(
    selections: &mut stoat::selection::SelectionsCollection,
    snapshot: &stoat::multi_buffer::MultiBufferSnapshot,
    target: Range<usize>,
) {
    let new_start = snapshot.anchor_at(target.start, Bias::Right);
    let new_end = snapshot.anchor_at(target.end, Bias::Left);
    let new_disjoint: Vec<Selection<Anchor>> = selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let mut new = sel.clone();
            new.start = new_start;
            new.end = new_end;
            new.reversed = false;
            new.goal = SelectionGoal::None;
            new
        })
        .collect();
    selections.replace_with(new_disjoint, snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, Entity, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_language::{Language, LanguageRegistry, SyntaxMap};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::Rope;

    fn rust_language() -> Arc<Language> {
        LanguageRegistry::standard()
            .find_by_name("rust")
            .expect("rust grammar")
    }

    fn build_syntax_map(text: &str, lang: Arc<Language>) -> SyntaxMap {
        let rope = Rope::from(text);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1).expect("reparse");
        map
    }

    fn new_editor_with_syntax(
        cx: &mut TestAppContext,
        text: &str,
        lang: Option<Arc<Language>>,
    ) -> (Entity<Buffer>, Entity<Editor>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        if let Some(lang) = lang {
            let map = build_syntax_map(text, lang);
            buffer.update(cx, |b, cx| b.set_syntax_map(Some(map), cx));
        }
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        let editor = cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        });
        (buffer, editor)
    }

    fn seed_cursor(editor: &Entity<Editor>, cx: &mut TestAppContext, offset: usize) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    fn seed_range(editor: &Entity<Editor>, cx: &mut TestAppContext, range: Range<usize>) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.anchor_at(range.start, Bias::Right);
            let end = snapshot.anchor_at(range.end, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start,
                    end,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    fn primary_range(editor: &Entity<Editor>, cx: &mut TestAppContext) -> (usize, usize) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = ed
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .expect("at least one selection");
            (
                snapshot.resolve_anchor(&sel.start),
                snapshot.resolve_anchor(&sel.end),
            )
        })
    }

    fn selection_ranges(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Vec<(usize, usize)> {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| {
                    (
                        snapshot.resolve_anchor(&s.start),
                        snapshot.resolve_anchor(&s.end),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn expand_selection_walks_to_enclosing_node() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_expand_selection(1, cx));

        let (start, end) = primary_range(&editor, &mut cx);
        assert!(end > start, "selection grew from cursor");
        assert!(end - start >= 5, "covers at least the identifier");
    }

    #[test]
    fn expand_then_shrink_restores_original_range() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_expand_selection(2, cx));
        let expanded = primary_range(&editor, &mut cx);

        editor.update(&mut cx, |ed, cx| ed.handle_shrink_selection(1, cx));
        let after_one_shrink = primary_range(&editor, &mut cx);
        assert_ne!(after_one_shrink, expanded);

        editor.update(&mut cx, |ed, cx| ed.handle_shrink_selection(1, cx));
        let after_two_shrinks = primary_range(&editor, &mut cx);
        assert_eq!(after_two_shrinks, (3, 3));
    }

    #[test]
    fn shrink_with_empty_history_is_noop() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_shrink_selection(1, cx));

        assert_eq!(primary_range(&editor, &mut cx), (3, 3));
    }

    #[test]
    fn expand_selection_without_syntax_map_is_noop() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, None);
        seed_cursor(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_expand_selection(1, cx));

        assert_eq!(primary_range(&editor, &mut cx), (3, 3));
    }

    #[test]
    fn select_next_sibling_walks_forward() {
        let mut cx = TestAppContext::single();
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_range(&editor, &mut cx, 0..src.find("\nfn b").expect("anchor"));

        editor.update(&mut cx, |ed, cx| {
            ed.handle_select_sibling(SiblingDir::Next, false, 1, cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        let selected = &src[start..end];
        assert!(
            selected.starts_with("fn b("),
            "got selection {selected:?} (range {start}..{end})",
        );
    }

    #[test]
    fn select_prev_sibling_walks_backward() {
        let mut cx = TestAppContext::single();
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let c_start = src.find("fn c").expect("fn c");
        seed_range(&editor, &mut cx, c_start..c_start + "fn c() {}".len());

        editor.update(&mut cx, |ed, cx| {
            ed.handle_select_sibling(SiblingDir::Prev, false, 1, cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        let selected = &src[start..end];
        assert!(
            selected.starts_with("fn b("),
            "got selection {selected:?} (range {start}..{end})",
        );
    }

    #[test]
    fn select_all_siblings_fans_into_each_sibling() {
        let mut cx = TestAppContext::single();
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let b_start = src.find("fn b").expect("fn b");
        seed_range(&editor, &mut cx, b_start..b_start + "fn b() {}".len());

        editor.update(&mut cx, |ed, cx| ed.handle_select_all_siblings(cx));

        let ranges = selection_ranges(&editor, &mut cx);
        assert_eq!(ranges.len(), 3, "one selection per function: {ranges:?}");
        for (start, end) in &ranges {
            assert!(end > start, "non-empty selection {start}..{end}");
        }
    }

    #[test]
    fn move_parent_node_start_lands_on_parent_open_byte() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() { let x = 1; }\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let x_offset = src.find('x').expect("x") + 1;
        seed_cursor(&editor, &mut cx, x_offset);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_parent_bound(NodeBound::Start, false, 1, cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        assert_eq!(start, end, "cursor collapses on parent start");
        assert!(start < x_offset, "parent start precedes x");
    }

    #[test]
    fn move_parent_node_end_lands_on_parent_close_byte() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() { let x = 1; }\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let x_offset = src.find('x').expect("x") + 1;
        seed_cursor(&editor, &mut cx, x_offset);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_parent_bound(NodeBound::End, false, 1, cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        assert_eq!(start, end, "cursor collapses on parent end");
        assert!(end > x_offset, "parent end follows x");
    }

    #[test]
    fn goto_next_function_jumps_to_next_fn_keyword() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Function, NavDirection::Next, cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        assert_eq!(start, end);
        assert_eq!(&src[start..start + 8], "fn beta(");
    }

    #[test]
    fn goto_prev_function_jumps_backward() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, src.len());

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Function, NavDirection::Prev, cx)
        });

        let (start, _) = primary_range(&editor, &mut cx);
        assert_eq!(&src[start..start + 9], "fn gamma(");
    }

    #[test]
    fn goto_next_class_finds_struct_enum_trait_impl() {
        let mut cx = TestAppContext::single();
        let src = "struct Foo {}\nenum Bar { A }\ntrait Baz {}\nimpl Foo {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Class, NavDirection::Next, cx)
        });
        let (a, _) = primary_range(&editor, &mut cx);
        assert!(
            src[a..].starts_with("enum Bar"),
            "first hit: {:?}",
            &src[a..],
        );

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Class, NavDirection::Next, cx)
        });
        let (b, _) = primary_range(&editor, &mut cx);
        assert!(
            src[b..].starts_with("trait Baz"),
            "second hit: {:?}",
            &src[b..],
        );

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Class, NavDirection::Next, cx)
        });
        let (c, _) = primary_range(&editor, &mut cx);
        assert!(
            src[c..].starts_with("impl Foo"),
            "third hit: {:?}",
            &src[c..],
        );
    }

    #[test]
    fn goto_textobject_no_match_is_noop() {
        let mut cx = TestAppContext::single();
        let src = "fn only() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let end = src.len();
        seed_cursor(&editor, &mut cx, end);
        let before = primary_range(&editor, &mut cx);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Function, NavDirection::Next, cx)
        });

        assert_eq!(primary_range(&editor, &mut cx), before);
    }

    #[test]
    fn goto_textobject_without_syntax_map_is_noop() {
        let mut cx = TestAppContext::single();
        let src = "fn only() {}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, None);
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_textobject(NavKind::Function, NavDirection::Next, cx)
        });

        assert_eq!(primary_range(&editor, &mut cx), (0, 0));
    }

    #[test]
    fn match_brackets_jumps_open_to_close() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "foo ( bar )", None);
        seed_cursor(&editor, &mut cx, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_match_brackets(cx));

        assert_eq!(primary_range(&editor, &mut cx), (10, 10));
    }

    #[test]
    fn match_brackets_jumps_close_to_open() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "foo ( bar )", None);
        seed_cursor(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| ed.handle_match_brackets(cx));

        assert_eq!(primary_range(&editor, &mut cx), (4, 4));
    }

    #[test]
    fn match_brackets_non_bracket_char_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "foo ( bar )", None);
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_match_brackets(cx));

        assert_eq!(primary_range(&editor, &mut cx), (0, 0));
    }

    #[test]
    fn match_brackets_unmatched_open_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "(((", None);
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_match_brackets(cx));

        assert_eq!(primary_range(&editor, &mut cx), (0, 0));
    }
}
