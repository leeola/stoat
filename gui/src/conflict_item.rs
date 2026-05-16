use crate::{
    buffer::Buffer,
    diff_map::DiffMap,
    display_map::DisplayMap,
    editor::{Editor, EditorMode},
    globals::ExecutorGlobal,
    item::{DeserializeSnafu, ItemError, ItemView},
    multi_buffer::MultiBuffer,
};
use gpui::{
    div, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window,
};
use serde_json::Value;
use std::{ops::Range, path::PathBuf};
use stoat::{buffer::BufferId, host::ConflictedFile};

const MISSING_SIDE_PLACEHOLDER: &str = "(file not present)\n";

/// Which side of a conflict block fills the resolution. Selected by
/// `ConflictTakeOurs` / `ConflictTakeTheirs` on the workspace dispatch
/// path; consumed by [`ConflictItem::take_side`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConflictSide {
    Ours,
    Theirs,
}

/// Pane-hosted 3-way merge view for one [`ConflictedFile`].
///
/// Layout: three side-by-side read-only editors (ancestor / ours /
/// theirs) on top, one writable "result" editor at the bottom. The
/// result editor's buffer starts populated with a standard
/// conflict-marker block (`<<<<<<< ours / ======= / >>>>>>> theirs`)
/// over `ours` and `theirs`; the user edits the result freely to
/// produce the final resolved content.
pub struct ConflictItem {
    rel_path: PathBuf,
    ancestor: SideView,
    ours: SideView,
    theirs: SideView,
    result: SideView,
}

/// The handles backing one editor pane. The [`Buffer`] handle is
/// retained alongside the [`Editor`] so callers can read or rewrite
/// the buffer text without having to navigate through the editor's
/// [`MultiBuffer`].
struct SideView {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
}

impl ConflictItem {
    /// Build a [`ConflictItem`] from `file`. Missing sides (e.g.
    /// `ours = None` for a deletion) render with a placeholder so the
    /// editor pane stays visible.
    ///
    /// Reads [`ExecutorGlobal`] for the per-buffer [`DisplayMap`]; the
    /// caller must install it before constructing the entity.
    pub fn from_conflicted_file(file: ConflictedFile, cx: &mut Context<'_, Self>) -> Self {
        let ancestor_text = file.ancestor.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let ours_text = file.ours.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let theirs_text = file.theirs.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let result_text = format_conflict_markers(file.ours.as_deref(), file.theirs.as_deref());

        let ancestor = build_side_editor(ancestor_text, cx);
        let ours = build_side_editor(ours_text, cx);
        let theirs = build_side_editor(theirs_text, cx);
        let result = build_side_editor(&result_text, cx);

        Self {
            rel_path: file.path,
            ancestor,
            ours,
            theirs,
            result,
        }
    }

    /// Replace the conflict block enclosing the result editor's
    /// primary cursor with `side`'s content. The block must be a
    /// well-formed `<<<<<<< / ======= / >>>>>>>` triple; mismatched
    /// or absent markers leave the buffer untouched.
    pub(crate) fn take_side(&self, side: ConflictSide, cx: &mut Context<'_, Self>) {
        let cursor = {
            let editor = self.result.editor.read(cx);
            let snapshot = editor.multi_buffer().read(cx).snapshot();
            let Some(primary) = editor
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
            else {
                return;
            };
            snapshot.resolve_anchor(&primary.head())
        };
        let text = self.result_buffer_text(cx);
        let Some(block) = find_enclosing_conflict_block(&text, cursor) else {
            return;
        };
        let replacement_range = match side {
            ConflictSide::Ours => block.ours,
            ConflictSide::Theirs => block.theirs,
        };
        let replacement = text[replacement_range].to_owned();
        self.result.buffer.update(cx, |buf, cx| {
            buf.edit(block.range, &replacement, cx);
        });
    }

    /// Snapshot of the result buffer's text -- the resolved content
    /// the user has produced so far.
    pub(crate) fn result_buffer_text(&self, cx: &App) -> String {
        self.result.buffer.read(cx).text()
    }

    /// True while the result buffer still carries at least one
    /// complete `<<<<<<< / ======= / >>>>>>>` block.
    pub(crate) fn has_unresolved_conflicts(&self, cx: &App) -> bool {
        has_any_conflict_block(&self.result_buffer_text(cx))
    }
}

/// Byte offsets of a `<<<<<<< / ======= / >>>>>>>` block in a buffer.
/// `range` covers the entire marker block; `ours` and `theirs` cover
/// the content of each side, both bounded by `\n` line boundaries so
/// the slices stay UTF-8 valid.
struct ConflictBlock {
    range: Range<usize>,
    ours: Range<usize>,
    theirs: Range<usize>,
}

fn has_any_conflict_block(text: &str) -> bool {
    enum S {
        Outside,
        InOurs,
        InTheirs,
    }
    let mut state = S::Outside;
    for line in text.lines() {
        match state {
            S::Outside if line.starts_with("<<<<<<<") => state = S::InOurs,
            S::InOurs if line.starts_with("=======") => state = S::InTheirs,
            S::InTheirs if line.starts_with(">>>>>>>") => return true,
            _ => {},
        }
    }
    false
}

fn find_enclosing_conflict_block(text: &str, cursor: usize) -> Option<ConflictBlock> {
    enum State {
        Outside,
        InOurs {
            block_start: usize,
            ours_start: usize,
        },
        InTheirs {
            block_start: usize,
            ours: Range<usize>,
            theirs_start: usize,
        },
    }

    let mut state = State::Outside;
    let mut pos = 0usize;
    loop {
        let nl = text[pos..].find('\n');
        let line_end = nl.map(|p| pos + p).unwrap_or(text.len());
        let next_line_start = nl.map(|_| line_end + 1).unwrap_or(text.len());
        let line = &text[pos..line_end];

        match &state {
            State::Outside => {
                if line.starts_with("<<<<<<<") {
                    state = State::InOurs {
                        block_start: pos,
                        ours_start: next_line_start,
                    };
                }
            },
            State::InOurs {
                block_start,
                ours_start,
            } => {
                if line.starts_with("=======") {
                    state = State::InTheirs {
                        block_start: *block_start,
                        ours: *ours_start..pos,
                        theirs_start: next_line_start,
                    };
                }
            },
            State::InTheirs {
                block_start,
                ours,
                theirs_start,
            } => {
                if line.starts_with(">>>>>>>") {
                    let block_end = next_line_start;
                    if (*block_start..block_end).contains(&cursor) {
                        return Some(ConflictBlock {
                            range: *block_start..block_end,
                            ours: ours.clone(),
                            theirs: *theirs_start..pos,
                        });
                    }
                    state = State::Outside;
                }
            },
        }

        if nl.is_none() {
            break;
        }
        pos = next_line_start;
    }
    None
}

fn build_side_editor(text: &str, cx: &mut Context<'_, ConflictItem>) -> SideView {
    let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), text));
    let multi_buffer = {
        let buffer = buffer.clone();
        cx.new(|cx| MultiBuffer::singleton(buffer, cx))
    };

    let executor = cx.global::<ExecutorGlobal>().0.clone();
    let display_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DisplayMap::new(buffer, executor, cx))
    };
    let diff_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DiffMap::new(buffer, cx))
    };

    let editor =
        cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));

    SideView { buffer, editor }
}

fn format_conflict_markers(ours: Option<&str>, theirs: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("<<<<<<< ours\n");
    out.push_str(ours.unwrap_or(""));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("=======\n");
    out.push_str(theirs.unwrap_or(""));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(">>>>>>> theirs\n");
    out
}

impl Render for ConflictItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let top = div()
            .flex()
            .flex_row()
            .flex_1()
            .child(div().flex_1().child(self.ancestor.editor.clone()))
            .child(div().flex_1().child(self.ours.editor.clone()))
            .child(div().flex_1().child(self.theirs.editor.clone()));
        let bottom = div().flex_1().child(self.result.editor.clone());
        div().flex().flex_col().size_full().child(top).child(bottom)
    }
}

impl ItemView for ConflictItem {
    fn tab_label(&self, _cx: &App) -> SharedString {
        let name = self
            .rel_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| self.rel_path.display().to_string());
        format!("Conflict: {name}").into()
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "ConflictItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn make_file(
        path: &str,
        ancestor: Option<&str>,
        ours: Option<&str>,
        theirs: Option<&str>,
    ) -> ConflictedFile {
        ConflictedFile {
            path: PathBuf::from(path),
            ancestor: ancestor.map(String::from),
            ours: ours.map(String::from),
            theirs: theirs.map(String::from),
        }
    }

    #[test]
    fn tab_label_uses_file_path() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("src/foo.rs", None, Some("a\n"), Some("b\n")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Conflict: foo.rs"));
        });
    }

    #[test]
    fn result_buffer_is_populated_with_conflict_markers() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("ours-line\n"), Some("theirs-line\n")),
                    cx,
                )
            })
        });
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(
            text,
            "<<<<<<< ours\nours-line\n=======\ntheirs-line\n>>>>>>> theirs\n",
        );
    }

    #[test]
    fn missing_ancestor_uses_placeholder() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        let ancestor_text = item.read_with(&cx, |item, cx| item.ancestor.buffer.read(cx).text());
        assert_eq!(ancestor_text, MISSING_SIDE_PLACEHOLDER);
    }

    #[test]
    fn is_dirty_is_false_initially() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        let err = item.update(&mut cx, |_, cx| {
            ConflictItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn missing_ours_yields_empty_ours_section_in_result() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, None, Some("theirs-line\n")),
                    cx,
                )
            })
        });
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "<<<<<<< ours\n=======\ntheirs-line\n>>>>>>> theirs\n",);
    }

    #[test]
    fn find_enclosing_conflict_block_single_block_cursor_in_ours() {
        let text = "<<<<<<< ours\nalpha\n=======\nbeta\n>>>>>>> theirs\n";
        let cursor = text.find("alpha").expect("ours line present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in block");
        assert_eq!(&text[block.range.clone()], text);
        assert_eq!(&text[block.ours], "alpha\n");
        assert_eq!(&text[block.theirs], "beta\n");
    }

    #[test]
    fn find_enclosing_conflict_block_no_marker_returns_none() {
        assert!(find_enclosing_conflict_block("just plain text\nno markers\n", 4).is_none());
    }

    #[test]
    fn find_enclosing_conflict_block_cursor_between_blocks_returns_none() {
        let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\nbetween\n<<<<<<< ours\nc\n=======\nd\n>>>>>>> theirs\n";
        let cursor = text.find("between").expect("between marker present");
        assert!(find_enclosing_conflict_block(text, cursor).is_none());
    }

    #[test]
    fn find_enclosing_conflict_block_returns_second_block_when_cursor_inside() {
        let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\nx\n<<<<<<< ours\nc\n=======\nd\n>>>>>>> theirs\n";
        let cursor = text.find('c').expect("second-block ours present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in second block");
        assert_eq!(&text[block.ours], "c\n");
        assert_eq!(&text[block.theirs], "d\n");
    }

    #[test]
    fn find_enclosing_conflict_block_handles_branch_name_suffix() {
        let text = "<<<<<<< feature-x\nfoo\n=======\nbar\n>>>>>>> main\n";
        let cursor = text.find("foo").expect("ours line present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in block");
        assert_eq!(&text[block.ours], "foo\n");
        assert_eq!(&text[block.theirs], "bar\n");
    }

    fn place_cursor_at(editor: &Entity<Editor>, offset: usize, cx: &mut TestAppContext) {
        use stoat_text::{Bias, Selection, SelectionGoal};
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
            cx.notify();
        });
    }

    #[test]
    fn take_ours_replaces_block_with_ours_content() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "alpha\n");
    }

    #[test]
    fn take_theirs_replaces_block_with_theirs_content() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Theirs, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "beta\n");
    }

    #[test]
    fn has_any_conflict_block_returns_true_for_complete_block() {
        assert!(has_any_conflict_block(
            "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n"
        ));
    }

    #[test]
    fn has_any_conflict_block_returns_false_without_markers() {
        assert!(!has_any_conflict_block("just plain text\n"));
    }

    #[test]
    fn has_any_conflict_block_returns_false_for_partial_markers() {
        assert!(!has_any_conflict_block(
            "<<<<<<< ours\na\n=======\nb\n(no end marker)\n"
        ));
        assert!(!has_any_conflict_block(
            "<<<<<<< ours\na\n(no separator)\n>>>>>>> theirs\n"
        ));
        assert!(!has_any_conflict_block(
            "(only end marker)\n>>>>>>> theirs\n"
        ));
    }

    #[test]
    fn has_unresolved_conflicts_is_true_for_fresh_item() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, cx| {
            assert!(item.has_unresolved_conflicts(cx));
        });
    }

    #[test]
    fn has_unresolved_conflicts_is_false_after_take_side() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        item.read_with(&cx, |item, cx| {
            assert!(!item.has_unresolved_conflicts(cx));
        });
    }

    #[test]
    fn take_side_is_noop_when_cursor_outside_any_block() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("a\n"), Some("b\n")),
                    cx,
                )
            })
        });
        item.update(&mut cx, |item, cx| {
            item.result.buffer.update(cx, |buf, cx| {
                let text = buf.text();
                buf.edit(0..text.len(), "no markers here\n", cx);
            });
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "no markers here\n");
    }
}
