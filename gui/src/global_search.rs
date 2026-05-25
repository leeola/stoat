//! Global-search picker delegate.
//!
//! The picker's query editor doubles as a regex input: every
//! keystroke spawns a workspace scan via
//! [`stoat::global_search::perform_search`] on a blocking worker,
//! and the resulting [`SearchMatch`] list replaces the picker's
//! entries. Confirm opens the matched file in the focused pane and
//! jumps the primary cursor to the byte offset of the match.
//!
//! Each background scan is tagged with a monotonically-increasing
//! version. Late-arriving results whose version no longer matches
//! the delegate's current cursor are dropped so stale scans never
//! overwrite a more recent one. Invalid regex input produces an
//! empty result silently rather than surfacing a parse error.

use crate::{
    buffer::Buffer,
    editor::Editor,
    globals::{ExecutorGlobal, FsHostGlobal},
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, IntoElement, ParentElement, Styled, Task,
    WeakEntity, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    global_search::{perform_search, SearchMatch},
    host::FsHost,
};
use stoat_scheduler::Executor;
use stoat_text::{Bias, Selection, SelectionGoal};

pub struct GlobalSearchDelegate {
    workspace: WeakEntity<Workspace>,
    fs_host: Arc<dyn FsHost>,
    executor: Executor,
    git_root: PathBuf,
    entries: Vec<SearchMatch>,
    selected: usize,
    /// Monotonic counter bumped on every [`PickerDelegate::update_matches`]
    /// call. The async task captures the version at spawn time; when
    /// the scan returns, the delegate accepts the result only if its
    /// captured version still equals [`Self::query_version`]. Stale
    /// scans -- typical when the user types faster than scans
    /// complete -- are discarded.
    query_version: u64,
}

impl GlobalSearchDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        fs_host: Arc<dyn FsHost>,
        executor: Executor,
        git_root: PathBuf,
    ) -> Self {
        Self {
            workspace,
            fs_host,
            executor,
            git_root,
            entries: Vec::new(),
            selected: 0,
            query_version: 0,
        }
    }

    fn selected_entry(&self) -> Option<&SearchMatch> {
        self.entries.get(self.selected)
    }
}

impl PickerDelegate for GlobalSearchDelegate {
    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.entries.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        self.query_version = self.query_version.wrapping_add(1);
        let version = self.query_version;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.entries.clear();
            self.selected = 0;
            cx.notify();
            return Task::ready(());
        }
        let pattern = trimmed.to_string();
        let fs_host = self.fs_host.clone();
        let git_root = self.git_root.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.executor
            .spawn_blocking(move || {
                let result = perform_search(&*fs_host, &git_root, &pattern).unwrap_or_default();
                let _ = tx.send(result);
            })
            .detach();
        cx.spawn(async move |this, cx| {
            let Ok(scan) = rx.await else {
                return;
            };
            let _ = this.update(cx, |picker, cx| {
                let delegate = picker.delegate_mut();
                if delegate.query_version != version {
                    return;
                }
                delegate.entries = scan;
                if delegate.selected >= delegate.entries.len() {
                    delegate.selected = delegate.entries.len().saturating_sub(1);
                }
                cx.notify();
            });
        })
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.open_paths(std::slice::from_ref(&entry.path), cx);
            let Some(editor) = workspace
                .buffer_for_path(&entry.path, cx)
                .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
            else {
                return;
            };
            set_cursor_to_offset(&editor, entry.offset, cx);
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let display = render_row(entry, &self.git_root);
        let color = cx.theme().statusbar_text;
        let mut row = div().px_2().text_color(color).child(display);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

/// Open the global-search picker as the workspace's active modal.
pub fn open_global_search(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let fs_host = cx.global::<FsHostGlobal>().0.clone();
    let executor = cx.global::<ExecutorGlobal>().0.clone();
    workspace.toggle_modal::<Picker<GlobalSearchDelegate>, _>(window, cx, move |window, cx| {
        let delegate = GlobalSearchDelegate::new(weak_workspace, fs_host, executor, git_root);
        Picker::new(delegate, window, cx)
    });
}

fn render_row(entry: &SearchMatch, git_root: &Path) -> String {
    let path = display_path(&entry.path, git_root);
    format!("{path}:{}:{}  {}", entry.line, entry.column, entry.snippet)
}

fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn editor_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<Buffer>,
    cx: &gpui::App,
) -> Option<Entity<Editor>> {
    let target_id = buffer.entity_id();
    let pane_tree = workspace.pane_tree().read(cx);
    for pane_id in pane_tree.split_pane_ids() {
        let pane = pane_tree.pane(pane_id)?;
        for item in pane.read(cx).items() {
            let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                continue;
            };
            let mb_singleton = editor
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .cloned();
            if mb_singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                return Some(editor);
            }
        }
    }
    None
}

fn set_cursor_to_offset(editor: &Entity<Editor>, offset: usize, cx: &mut Context<'_, Workspace>) {
    editor.update(cx, |ed, cx| {
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = ed
            .selections()
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        ed.selections_mut().replace_with(vec![selection], &snapshot);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{TestAppContext, VisualTestContext};
    use stoat::host::{fake::FakeFs, FsWatchHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fake_fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
        });
    }

    fn fake_fs_with_files(files: &[(&str, &str)]) -> Arc<FakeFs> {
        let fs = FakeFs::new();
        let root = Path::new("/repo");
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        Arc::new(fs)
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, fake_fs: Arc<FakeFs>) -> Harness<'a> {
        install_globals(cx, fake_fs);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness { workspace, vcx }
    }

    fn type_query(h: &mut Harness<'_>, query: &str) {
        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("global search picker is open");
        let buffer = picker.read_with(h.vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line editor has singleton buffer")
                .clone()
        });
        buffer.update(h.vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, query, cx);
        });
    }

    #[test]
    fn open_global_search_makes_picker_modal_active() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<GlobalSearchDelegate>>()
                .is_some()
        });
        assert!(active, "global search picker should be the active modal");
    }

    #[test]
    fn typing_query_populates_entries_from_scan() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[
            ("a.rs", "fn alpha() {}\nfn beta() {}\n"),
            ("b.rs", "fn alpha_two() {}\n"),
        ]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        type_query(&mut h, "alpha");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let mut paths: Vec<String> = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| {
                    e.path
                        .file_name()
                        .expect("entry path has file name")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        });
        paths.sort();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn invalid_regex_leaves_entries_empty_silently() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "hello")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        type_query(&mut h, "[unclosed");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let count = picker.read_with(h.vcx, |p, _| p.delegate().entries.len());
        assert_eq!(count, 0);
    }

    #[test]
    fn empty_query_clears_entries() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "hello")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();
        type_query(&mut h, "hello");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let populated = picker.read_with(h.vcx, |p, _| p.delegate().entries.len());
        assert_eq!(populated, 1);

        type_query(&mut h, "");
        h.vcx.run_until_parked();
        let count = picker.read_with(h.vcx, |p, _| p.delegate().entries.len());
        assert_eq!(count, 0);
    }

    #[test]
    fn confirm_opens_path_and_jumps_to_offset() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        type_query(&mut h, "beta");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let expected_offset = picker.read_with(h.vcx, |p, _| {
            let entries = &p.delegate().entries;
            assert_eq!(entries.len(), 1, "expected one match for beta");
            entries[0].offset
        });

        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx);
        });
        h.vcx.run_until_parked();

        let path = PathBuf::from("/repo/a.rs");
        let opened_buffer = h
            .workspace
            .read_with(h.vcx, |w, cx| w.buffer_for_path(&path, cx).is_some());
        assert!(opened_buffer, "buffer should be open after confirm");

        let editor = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                let buffer = w.buffer_for_path(&path, cx).expect("buffer for path");
                editor_for_buffer(w, &buffer, cx)
            })
            .expect("editor for opened buffer");
        let cursor_offset = editor.read_with(h.vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = ed
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .expect("at least one selection");
            snapshot.resolve_anchor(&sel.head())
        });
        assert_eq!(cursor_offset, expected_offset);
    }
}
