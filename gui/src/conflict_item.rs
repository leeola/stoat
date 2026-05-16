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
use std::path::PathBuf;
use stoat::{buffer::BufferId, host::ConflictedFile};

const MISSING_SIDE_PLACEHOLDER: &str = "(file not present)\n";

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
}
