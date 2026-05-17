//! Helix-parity textobject selection: `m a <type>` / `m i <type>`.
//!
//! [`stoat_action::SelectTextobjectAround`] /
//! [`stoat_action::SelectTextobjectInner`] arm a single-char chord
//! through
//! [`crate::input_state_machine::InputStateMachine::arm_textobject_select`].
//! The next char keystroke synthesizes
//! [`crate::actions::ApplyTextobjectChar`], which the workspace
//! dispatches into [`Editor::handle_apply_textobject`]. Type chars
//! mirror Helix defaults: `p` (paragraph), `f` (function), `t`
//! (class), `a` (parameter), `c` (comment).
//!
//! Tree-sitter-driven types use the active layer's
//! [`stoat_language::Language::textobjects_query`] and pick the
//! smallest capture containing the cursor. Buffers whose language
//! has no `textobjects.scm`, or whose query lacks the requested
//! capture, are no-ops for those types. Paragraph is line-based
//! and works on any buffer.

use crate::{editor::Editor, workspace::Workspace};
use gpui::{Context, Entity};
use std::ops::Range;
use stoat_language::{find_smallest_capture_at, SyntaxLayer, SyntaxMap};
use stoat_text::{Anchor, Bias, Point, Rope, Selection, SelectionGoal};

/// Around / Inner selection mode for the active textobject chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextobjectMode {
    Around,
    Inner,
}

impl TextobjectMode {
    fn capture_suffix(self) -> &'static str {
        match self {
            TextobjectMode::Around => "around",
            TextobjectMode::Inner => "inside",
        }
    }
}

fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

pub fn handle_select_textobject_around(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    workspace
        .input_state_machine()
        .clone()
        .update(cx, |sm, cx| {
            sm.arm_textobject_select(TextobjectMode::Around, cx)
        });
}

pub fn handle_select_textobject_inner(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    workspace
        .input_state_machine()
        .clone()
        .update(cx, |sm, cx| {
            sm.arm_textobject_select(TextobjectMode::Inner, cx)
        });
}

pub fn handle_apply_textobject_char(
    workspace: &mut Workspace,
    mode: TextobjectMode,
    ch: char,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    editor.update(cx, |ed, cx| ed.handle_apply_textobject(mode, ch, cx));
}

impl Editor {
    /// Resolve the `mode`/`ch` chord to a target byte range and
    /// install it as the primary selection. Unknown type chars and
    /// ranges that cannot be resolved are no-ops; multi-excerpt
    /// buffers are not yet supported and are also no-ops.
    pub fn handle_apply_textobject(
        &mut self,
        mode: TextobjectMode,
        ch: char,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer().read(cx).snapshot();
        let cursor = {
            let head = self.selections().newest_anchor().head();
            snapshot.resolve_anchor(&head)
        };

        let Some(singleton) = self.multi_buffer().read(cx).as_singleton().cloned() else {
            return;
        };

        let target = match ch {
            'p' => {
                let buffer = singleton.read(cx);
                buffer.read(|b| find_textobject_paragraph(b.rope(), cursor, mode))
            },
            'f' | 't' | 'a' | 'c' => {
                let kind = match ch {
                    'f' => "function",
                    't' => "class",
                    'a' => "parameter",
                    'c' => "comment",
                    _ => unreachable!(),
                };
                find_textobject_treesitter(singleton.read(cx), cursor, kind, mode)
            },
            _ => None,
        };

        let Some(range) = target else {
            return;
        };

        let new_snapshot = self.multi_buffer().read(cx).snapshot();
        let new_start = new_snapshot.anchor_at(range.start, Bias::Right);
        let new_end = new_snapshot.anchor_at(range.end, Bias::Left);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections()
            .all_anchors()
            .iter()
            .map(|sel| Selection {
                id: sel.id,
                start: new_start,
                end: new_end,
                reversed: false,
                goal: SelectionGoal::None,
            })
            .collect();
        self.selections_mut()
            .replace_with(new_disjoint, &new_snapshot);
        cx.emit(crate::editor::EditorEvent::Changed);
        cx.notify();
    }
}

fn find_textobject_treesitter(
    buffer: &crate::buffer::Buffer,
    cursor: usize,
    kind: &str,
    mode: TextobjectMode,
) -> Option<Range<usize>> {
    let syntax_map = buffer.syntax_map()?;
    let layer = deepest_containing_layer(syntax_map, cursor, cursor)?;
    let query = layer.language.textobjects_query.as_ref()?;
    let capture_name = format!("{kind}.{}", mode.capture_suffix());
    buffer.read(|b| {
        find_smallest_capture_at(
            query,
            layer.tree.root_node(),
            b.rope(),
            &capture_name,
            cursor,
        )
    })
}

fn deepest_containing_layer(map: &SyntaxMap, start: usize, end: usize) -> Option<&SyntaxLayer> {
    map.snapshot().iter_layers().fold(None, |acc, layer| {
        let lstart = layer.start_offset as usize;
        let lend = layer.end_offset as usize;
        if lstart <= start && lend >= end {
            match acc {
                Some(prev) if prev.depth >= layer.depth => acc,
                _ => Some(layer),
            }
        } else {
            acc
        }
    })
}

/// Line-based paragraph textobject. Walks lines around `cursor`
/// finding the run of non-blank lines; Around mode includes the
/// trailing blank-line run, Inner mode trims trailing blanks.
fn find_textobject_paragraph(
    rope: &Rope,
    cursor: usize,
    mode: TextobjectMode,
) -> Option<Range<usize>> {
    let max_row = rope.max_point().row;
    let cursor_row = rope.offset_to_point(cursor).row;
    if rope.is_empty() {
        return None;
    }

    if rope.line_len(cursor_row) == 0 {
        let mut probe = cursor_row;
        let mut found = None;
        while probe > 0 {
            probe -= 1;
            if rope.line_len(probe) > 0 {
                found = Some(probe);
                break;
            }
        }
        if found.is_none() {
            let mut probe = cursor_row;
            while probe < max_row {
                probe += 1;
                if rope.line_len(probe) > 0 {
                    found = Some(probe);
                    break;
                }
            }
        }
        let anchor_row = found?;
        return paragraph_range_starting_from(rope, anchor_row, mode, max_row);
    }

    let mut start_row = cursor_row;
    while start_row > 0 && rope.line_len(start_row - 1) > 0 {
        start_row -= 1;
    }
    let mut end_row = cursor_row;
    while end_row < max_row && rope.line_len(end_row + 1) > 0 {
        end_row += 1;
    }

    let start = rope.point_to_offset(Point::new(start_row, 0));
    let inner_end = end_of_line_offset(rope, end_row);
    match mode {
        TextobjectMode::Inner => Some(start..inner_end),
        TextobjectMode::Around => {
            let mut tail_row = end_row;
            while tail_row < max_row && rope.line_len(tail_row + 1) == 0 {
                tail_row += 1;
            }
            let around_end = if tail_row == end_row {
                inner_end
            } else {
                end_of_line_offset(rope, tail_row)
            };
            Some(start..around_end)
        },
    }
}

fn paragraph_range_starting_from(
    rope: &Rope,
    anchor_row: u32,
    mode: TextobjectMode,
    max_row: u32,
) -> Option<Range<usize>> {
    let mut start_row = anchor_row;
    while start_row > 0 && rope.line_len(start_row - 1) > 0 {
        start_row -= 1;
    }
    let mut end_row = anchor_row;
    while end_row < max_row && rope.line_len(end_row + 1) > 0 {
        end_row += 1;
    }
    let start = rope.point_to_offset(Point::new(start_row, 0));
    let inner_end = end_of_line_offset(rope, end_row);
    match mode {
        TextobjectMode::Inner => Some(start..inner_end),
        TextobjectMode::Around => {
            let mut tail_row = end_row;
            while tail_row < max_row && rope.line_len(tail_row + 1) == 0 {
                tail_row += 1;
            }
            let around_end = if tail_row == end_row {
                inner_end
            } else {
                end_of_line_offset(rope, tail_row)
            };
            Some(start..around_end)
        },
    }
}

fn end_of_line_offset(rope: &Rope, row: u32) -> usize {
    let max = rope.max_point();
    if row >= max.row {
        rope.len()
    } else {
        rope.point_to_offset(Point::new(row + 1, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_language::{Language, LanguageRegistry, SyntaxMap};
    use stoat_scheduler::{Executor, TestScheduler};

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

    fn rope_of(s: &str) -> Rope {
        Rope::from(s)
    }

    #[test]
    fn paragraph_inner_selects_run_of_nonblank_lines() {
        let r = rope_of("alpha\nbeta\n\ngamma\n");
        let range =
            find_textobject_paragraph(&r, 2, TextobjectMode::Inner).expect("paragraph found");
        assert_eq!(range, 0..11);
    }

    #[test]
    fn paragraph_around_includes_trailing_blank() {
        let r = rope_of("alpha\nbeta\n\ngamma\n");
        let range =
            find_textobject_paragraph(&r, 2, TextobjectMode::Around).expect("paragraph found");
        assert_eq!(range, 0..12);
    }

    #[test]
    fn paragraph_cursor_on_blank_line_finds_neighbour() {
        let r = rope_of("alpha\n\nbeta\n");
        let range = find_textobject_paragraph(&r, 6, TextobjectMode::Inner)
            .expect("neighbour paragraph found");
        assert_eq!(range, 0..6);
    }

    #[test]
    fn paragraph_empty_buffer_is_none() {
        let r = rope_of("");
        assert_eq!(
            find_textobject_paragraph(&r, 0, TextobjectMode::Inner),
            None
        );
    }

    #[test]
    fn handle_apply_textobject_paragraph_inner_selects_run() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "alpha\nbeta\n\ngamma\n", None);
        seed_cursor(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_apply_textobject(TextobjectMode::Inner, 'p', cx)
        });

        assert_eq!(primary_range(&editor, &mut cx), (0, 11));
    }

    #[test]
    fn handle_apply_textobject_function_inner_selects_rust_body() {
        let mut cx = TestAppContext::single();
        let src = "fn alpha() {\n    let x = 1;\n}\n";
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, src, Some(rust_language()));
        let body_off = src.find("let").expect("body present");
        seed_cursor(&editor, &mut cx, body_off);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_apply_textobject(TextobjectMode::Inner, 'f', cx)
        });

        let (start, end) = primary_range(&editor, &mut cx);
        let span = &src[start..end];
        assert!(span.starts_with('{'), "got span {span:?}");
        assert!(span.contains("let x = 1;"));
        assert!(span.ends_with('}'));
    }

    #[test]
    fn handle_apply_textobject_unknown_char_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "alpha beta\n", None);
        seed_cursor(&editor, &mut cx, 3);
        let before = primary_range(&editor, &mut cx);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_apply_textobject(TextobjectMode::Inner, 'z', cx)
        });

        assert_eq!(primary_range(&editor, &mut cx), before);
    }

    #[test]
    fn handle_apply_textobject_no_query_for_json_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor_with_syntax(&mut cx, "{\"a\": 1}\n", None);
        seed_cursor(&editor, &mut cx, 5);
        let before = primary_range(&editor, &mut cx);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_apply_textobject(TextobjectMode::Inner, 'f', cx)
        });

        assert_eq!(primary_range(&editor, &mut cx), before);
    }
}
