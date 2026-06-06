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
    toast::Toast,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, AppContext, Context, DismissEvent, Entity, IntoElement, ParentElement, Styled,
    Task, WeakEntity, Window,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    global_search::{perform_search, SearchMatch},
    host::FsHost,
};
use stoat_action::{Action, ActionKind};
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
    /// The trimmed search pattern from the most recent non-empty
    /// scan. [`ActionKind::ReplaceAllInGlobalSearch`] recompiles it to
    /// locate match spans; reading the picker's query editor here would
    /// be a re-entrant borrow.
    query: String,
    /// When `true`, the replace input is shown below the query and
    /// receives typed text, and each match row previews the
    /// replacement. Toggled by [`ActionKind::ToggleReplaceInGlobalSearch`].
    replace_active: bool,
    /// Single-line editor holding the replacement pattern. Created
    /// lazily the first time replace mode is toggled on.
    replace_editor: Option<Entity<Editor>>,
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
            query: String::new(),
            replace_active: false,
            replace_editor: None,
        }
    }

    fn replace_text(&self, cx: &Context<'_, Picker<Self>>) -> String {
        let Some(editor) = self.replace_editor.as_ref() else {
            return String::new();
        };
        editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .map(|b| b.read(cx).text())
            .unwrap_or_default()
    }

    fn selected_entry(&self) -> Option<&SearchMatch> {
        self.entries.get(self.selected)
    }

    /// Rewrite every match across the searched files with the replace
    /// input's text and toast the outcome. No-op unless replace mode is
    /// active with a non-empty pattern. The work is deferred past the
    /// dispatch lease so the re-entrant workspace update does not panic.
    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<'_, Picker<Self>>) {
        if !self.replace_active || self.query.is_empty() {
            return;
        }
        let replacement = self.replace_text(cx);
        let pattern = self.query.clone();
        let mut seen = HashSet::new();
        let paths: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|entry| seen.insert(entry.path.clone()))
            .map(|entry| entry.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        let workspace = self.workspace.clone();
        window.defer(cx, move |_window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |w, cx| {
                let (matches, files) = w.replace_all_in_paths(&paths, &pattern, &replacement, cx);
                w.show_toast(
                    Toast::success(format!("Replaced {matches} matches across {files} files")),
                    cx,
                );
            });
        });
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
        self.query = pattern.clone();
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
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
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
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn handle_action(
        &mut self,
        action: &dyn Action,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> bool {
        match action.kind() {
            ActionKind::ToggleReplaceInGlobalSearch => {
                self.replace_active = !self.replace_active;
                if self.replace_active && self.replace_editor.is_none() {
                    self.replace_editor = Some(cx.new(|cx| Editor::single_line(window, cx)));
                }
                // The modal observer re-resolves the active text-input target
                // only on a modal-layer notification, which a delegate-internal
                // toggle does not raise. Notify it (deferred past the dispatch
                // lease) so input re-points to the replace editor, or back to the
                // query editor when toggled off.
                let workspace = self.workspace.clone();
                window.defer(cx, move |_window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update(cx, |w, cx| {
                            w.modal_layer().update(cx, |_, cx| cx.notify());
                        });
                    }
                });
                cx.notify();
                true
            },
            ActionKind::ReplaceAllInGlobalSearch => {
                self.replace_all(window, cx);
                true
            },
            _ => false,
        }
    }

    fn text_input_editor(&self) -> Option<WeakEntity<Editor>> {
        if self.replace_active {
            self.replace_editor.as_ref().map(Entity::downgrade)
        } else {
            None
        }
    }

    fn render_header(&self, cx: &mut Context<'_, Picker<Self>>) -> Option<AnyElement> {
        if !self.replace_active {
            return None;
        }
        let editor = self.replace_editor.as_ref()?;
        Some(
            div()
                .border_t_1()
                .border_color(cx.theme().border_inactive)
                .child(editor.clone())
                .into_any_element(),
        )
    }

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let display = render_row(entry, &self.git_root);
        let color = cx.theme().statusbar_text;
        let mut row = div().px_2().text_color(color);
        if self.replace_active {
            let replacement = self.replace_text(cx);
            row = row
                .flex()
                .flex_row()
                .items_center()
                .child(div().flex_grow().min_w_0().child(display))
                .child(
                    div()
                        .flex_none()
                        .px_2()
                        .text_color(cx.theme().muted_text)
                        .child(replacement),
                );
        } else {
            row = row.child(display);
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
    fn opening_global_search_sets_prompt_mode() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        let (open, mode) = h.workspace.read_with(h.vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.global_search_open(), sm.mode().to_string())
        });
        assert!(
            open,
            "global_search_open should be set while the picker is the active modal"
        );
        assert_eq!(
            mode, "prompt",
            "mode should be prompt while global search is active"
        );
    }

    #[test]
    fn closing_global_search_restores_mode() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();
        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        let (open, mode) = h.workspace.read_with(h.vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.global_search_open(), sm.mode().to_string())
        });
        assert!(
            !open,
            "global_search_open should clear after the picker closes"
        );
        assert_eq!(mode, "normal", "mode should restore to the prior value");
    }

    #[test]
    fn toggle_activates_replace_and_repoints_input() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("global search picker is open");

        assert!(
            !picker.read_with(h.vcx, |p, _| p.delegate().replace_active),
            "replace inactive before toggle"
        );

        picker.update_in(h.vcx, |p, window, cx| {
            p.handle_action(&stoat_action::ToggleReplaceInGlobalSearch, window, cx);
        });
        h.vcx.run_until_parked();

        let (active, replace_id) = picker.read_with(h.vcx, |p, _| {
            (
                p.delegate().replace_active,
                p.delegate().replace_editor.as_ref().map(|e| e.entity_id()),
            )
        });
        assert!(active, "replace active after toggle");
        assert!(replace_id.is_some(), "replace editor created on activate");

        let active_editor_id = h.workspace.read_with(h.vcx, |w, cx| {
            w.input_state_machine()
                .read(cx)
                .active_editor()
                .map(|e| e.entity_id())
        });
        assert_eq!(
            active_editor_id, replace_id,
            "typed text routes to the replace editor while replace mode is active"
        );
    }

    #[test]
    fn toggle_twice_deactivates_replace() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_global_search(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<GlobalSearchDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("global search picker is open");

        for _ in 0..2 {
            picker.update_in(h.vcx, |p, window, cx| {
                p.handle_action(&stoat_action::ToggleReplaceInGlobalSearch, window, cx);
            });
            h.vcx.run_until_parked();
        }

        assert!(
            !picker.read_with(h.vcx, |p, _| p.delegate().replace_active),
            "replace inactive after toggling twice"
        );
        assert!(
            picker.read_with(h.vcx, |p, _| p.delegate().text_input_editor().is_none()),
            "text_input_editor falls back to the query editor when replace is off"
        );
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

    fn set_replace_text(
        picker: &Entity<Picker<GlobalSearchDelegate>>,
        vcx: &mut VisualTestContext,
        text: &str,
    ) {
        let editor = picker
            .read_with(vcx, |p, _| p.delegate().replace_editor.clone())
            .expect("replace editor exists after toggle");
        let buffer = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line editor has singleton buffer")
                .clone()
        });
        buffer.update(vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, text, cx);
        });
    }

    fn open_and_read(h: &mut Harness<'_>, rel: &str) -> String {
        let path = Path::new("/repo").join(rel);
        h.workspace.update_in(h.vcx, |w, _window, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.workspace.read_with(h.vcx, |w, cx| {
            w.buffer_for_path(&path, cx)
                .expect("buffer open after replace")
                .read(cx)
                .text()
        })
    }

    #[test]
    fn replace_all_rewrites_matches_and_toasts() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[
            ("a.rs", "fn alpha() {}\nlet alpha = alpha;\n"),
            ("b.rs", "fn alpha() {}\n"),
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

        picker.update_in(h.vcx, |p, window, cx| {
            p.handle_action(&stoat_action::ToggleReplaceInGlobalSearch, window, cx);
        });
        h.vcx.run_until_parked();
        set_replace_text(&picker, h.vcx, "beta");

        picker.update_in(h.vcx, |p, window, cx| {
            p.handle_action(&stoat_action::ReplaceAllInGlobalSearch, window, cx);
        });
        h.vcx.run_until_parked();

        assert_eq!(
            open_and_read(&mut h, "a.rs"),
            "fn beta() {}\nlet beta = beta;\n"
        );
        assert_eq!(open_and_read(&mut h, "b.rs"), "fn beta() {}\n");

        let toasts = h.workspace.read_with(h.vcx, |w, cx| {
            w.toast_view()
                .read(cx)
                .toasts()
                .iter()
                .map(|t| t.text.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            toasts,
            vec!["Replaced 4 matches across 2 files".to_string()]
        );
    }
}
