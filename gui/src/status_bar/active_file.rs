use crate::{
    buffer::BufferEvent, editor::Editor, item::ItemHandle, status_bar::StatusItemView,
    theme::statusbar_text_color,
};
use gpui::{
    div, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    Window,
};
use std::path::PathBuf;

/// Status-bar item that surfaces the focused editor's
/// path-relative filename, plus a trailing ` [+]` when the
/// underlying buffer is dirty. Rebinds whenever the active pane
/// item changes; subscribes to the editor's singleton buffer so
/// `BufferEvent::Edited` / `Saved` / `Reloaded` keep the dirty
/// indicator in sync without polling.
pub struct ActiveFileLabel {
    workspace_root: PathBuf,
    filename: Option<SharedString>,
    dirty: bool,
    _buffer_subscription: Option<Subscription>,
}

impl ActiveFileLabel {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            filename: None,
            dirty: false,
            _buffer_subscription: None,
        }
    }

    pub fn filename(&self) -> Option<&SharedString> {
        self.filename.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn bind_to_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let (filename, buffer) = {
            let editor_ref = editor.read(cx);
            let filename = match editor_ref.file_path() {
                Some(path) => {
                    SharedString::from(stoat::paths::display_relative(path, &self.workspace_root))
                },
                None => SharedString::from("[scratch]"),
            };
            let buffer = editor_ref.multi_buffer().read(cx).as_singleton().cloned();
            (filename, buffer)
        };

        self.filename = Some(filename);
        self.dirty = buffer
            .as_ref()
            .map(|b| b.read(cx).is_dirty())
            .unwrap_or(false);
        self._buffer_subscription = buffer.map(|buffer| {
            cx.subscribe(&buffer, |this, buffer, event: &BufferEvent, cx| {
                if matches!(
                    event,
                    BufferEvent::Edited | BufferEvent::Saved | BufferEvent::Reloaded
                ) {
                    let next = buffer.read(cx).is_dirty();
                    if this.dirty != next {
                        this.dirty = next;
                        cx.notify();
                    }
                }
            })
        });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.filename.is_none() && !self.dirty && self._buffer_subscription.is_none() {
            return;
        }
        self.filename = None;
        self.dirty = false;
        self._buffer_subscription = None;
        cx.notify();
    }
}

impl Render for ActiveFileLabel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = self.filename.clone().map(|name| {
            let text = if self.dirty {
                SharedString::from(format!("{name} [+]"))
            } else {
                name
            };
            div()
                .px_2()
                .text_color(statusbar_text_color(cx))
                .child(text)
        });
        div().children(label)
    }
}

impl StatusItemView for ActiveFileLabel {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<'_, Self>,
    ) {
        let editor = active_pane_item.and_then(|item| item.to_any_view().downcast::<Editor>().ok());
        match editor {
            Some(editor) => self.bind_to_editor(&editor, cx),
            None => self.clear(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::{Editor, EditorMode},
        globals::ExecutorGlobal,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_editor(cx: &mut TestAppContext, path: Option<PathBuf>) -> Entity<Editor> {
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), ""));
            if let Some(p) = path.clone() {
                buffer.update(cx, |b, cx| b.set_file_path(Some(p), cx));
            }
            let multi_buffer = {
                let buffer = buffer.clone();
                cx.new(|cx| MultiBuffer::singleton(buffer, cx))
            };
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let display_map = {
                let buffer = buffer.clone();
                cx.new(|cx| DisplayMap::new(buffer, executor, cx))
            };
            let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
            let editor = cx
                .new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
            editor.update(cx, |ed, cx| ed.set_file_path(path, cx));
            editor
        })
    }

    fn new_label(cx: &mut TestAppContext, root: &str) -> Entity<ActiveFileLabel> {
        cx.update(|cx| cx.new(|_| ActiveFileLabel::new(PathBuf::from(root))))
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        label.read_with(&cx, |l, _| {
            assert_eq!(l.filename(), None);
            assert!(!l.is_dirty());
        });
    }

    #[test]
    fn binds_to_scratch_editor() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let editor = new_editor(&mut cx, None);
        let handle: Box<dyn ItemHandle> = Box::new(editor.clone());
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle), cx);
        });
        label.read_with(&cx, |l, _| {
            assert_eq!(l.filename(), Some(&SharedString::from("[scratch]")));
            assert!(!l.is_dirty());
        });
    }

    #[test]
    fn binds_to_file_editor_relative_to_workspace_root() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let editor = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/src/main.rs")));
        let handle: Box<dyn ItemHandle> = Box::new(editor.clone());
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle), cx);
        });
        label.read_with(&cx, |l, _| {
            assert_eq!(l.filename(), Some(&SharedString::from("src/main.rs")));
        });
    }

    #[test]
    fn rebinding_swaps_filename() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let first = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/a.rs")));
        let second = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/b.rs")));
        let handle_first: Box<dyn ItemHandle> = Box::new(first);
        let handle_second: Box<dyn ItemHandle> = Box::new(second);
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle_first), cx);
        });
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle_second), cx);
        });
        label.read_with(&cx, |l, _| {
            assert_eq!(l.filename(), Some(&SharedString::from("b.rs")));
        });
    }

    #[test]
    fn clear_drops_filename_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let editor = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/a.rs")));
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle), cx);
        });
        label.update(&mut cx, |l, cx| l.set_active_pane_item(None, cx));
        label.read_with(&cx, |l, _| {
            assert_eq!(l.filename(), None);
            assert!(!l.is_dirty());
        });
    }

    #[test]
    fn buffer_edited_flips_dirty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let editor = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/a.rs")));
        let buffer = editor
            .read_with(&cx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton buffer");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle), cx);
        });
        buffer.update(&mut cx, |b, cx| b.edit(0..0, "x", cx));
        cx.run_until_parked();
        label.read_with(&cx, |l, _| assert!(l.is_dirty()));
    }

    #[test]
    fn buffer_saved_clears_dirty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let label = new_label(&mut cx, "/tmp/repo");
        let editor = new_editor(&mut cx, None);
        let buffer = editor
            .read_with(&cx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton buffer");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        label.update(&mut cx, |l, cx| {
            l.set_active_pane_item(Some(&*handle), cx);
        });
        buffer.update(&mut cx, |b, cx| b.edit(0..0, "x", cx));
        cx.run_until_parked();
        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();
        label.read_with(&cx, |l, _| assert!(!l.is_dirty()));
    }
}
