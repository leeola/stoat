//! Persisted-workspace picker delegate.
//!
//! Lists every `.ron` workspace state file under the active git
//! root's `$XDG_STATE_HOME/stoat/workspaces/<git_root_hash>/`
//! directory. Each entry is parsed at picker-open time for its
//! display name, uid, and git_root; corrupt or unreadable files
//! are skipped with a `tracing::info!` and excluded. Confirm
//! rehydrates the active workspace via
//! [`Workspace::restore_state`], replacing panes, buffers, docks,
//! and Claude session in place.

use crate::{
    globals::FsHostGlobal,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{host::FsHost, workspace::WorkspaceUid};

pub struct WorkspacePickerEntry {
    pub path: PathBuf,
    pub label: String,
    pub is_current: bool,
}

pub struct WorkspacePickerDelegate {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn FsHost>,
    entries: Vec<WorkspacePickerEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl WorkspacePickerDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn FsHost>,
        entries: Vec<WorkspacePickerEntry>,
    ) -> Self {
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        let selected = entries.iter().position(|e| e.is_current).unwrap_or(0);
        Self {
            workspace,
            fs,
            entries,
            matches,
            selected,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            self.matches = (0..self.entries.len()).map(|i| (i, Vec::new())).collect();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }
        let items = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (i, render_row(entry)));
        let Some(mut ranked) = rank_matches(trimmed, items) else {
            self.matches.clear();
            self.selected = 0;
            return;
        };
        ranked.sort_by_key(|m| std::cmp::Reverse(m.score));
        self.matches = ranked
            .into_iter()
            .map(|m| (m.item, m.matched_indices))
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_entry(&self) -> Option<&WorkspacePickerEntry> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx)
    }
}

impl PickerDelegate for WorkspacePickerDelegate {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        self.query = query;
        self.refilter();
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let path = entry.path.clone();
        let is_current = entry.is_current;
        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            return;
        };
        if !is_current {
            let fs = self.fs.clone();
            // Defer past the keystroke observer's outer `Workspace::update`
            // lease so the re-entrant update does not panic.
            window.defer(cx, move |_window, cx| {
                workspace.update(cx, |ws, cx| {
                    if let Err(err) = ws.restore_state(&path, &*fs, cx) {
                        tracing::warn!(?err, ?path, "workspace picker restore_state failed");
                    }
                });
            });
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let display = render_row(entry);
        let color = cx.theme().statusbar_text;
        let runs = match_highlight_runs(
            &display,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(display)).with_highlights(runs);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(cx.theme().modal_selection);
        }
        row.into_any_element()
    }
}

fn render_row(entry: &WorkspacePickerEntry) -> String {
    if entry.is_current {
        format!("{} (current)", entry.label)
    } else {
        entry.label.clone()
    }
}

/// Open the persisted-workspace picker over `workspace`. No-op
/// when no persisted state exists under the workspace's git root
/// or when the [`FsHostGlobal`] is missing.
pub fn open_workspace_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(fs) = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone()) else {
        tracing::warn!("FsHostGlobal not installed; cannot open workspace picker");
        return;
    };
    let git_root = workspace.git_root().clone();
    let current_uid = workspace.uid();
    let entries = collect_entries(&git_root, current_uid, &*fs);
    if entries.is_empty() {
        return;
    }
    let weak_workspace = cx.weak_entity();
    workspace.toggle_modal::<Picker<WorkspacePickerDelegate>, _>(window, cx, move |window, cx| {
        let delegate = WorkspacePickerDelegate::new(weak_workspace, fs, entries);
        Picker::new(delegate, window, cx)
    });
}

fn collect_entries(
    git_root: &Path,
    current_uid: WorkspaceUid,
    fs: &dyn FsHost,
) -> Vec<WorkspacePickerEntry> {
    let files = match crate::workspace_persist::list_workspace_files(git_root, fs) {
        Ok(files) => files,
        Err(err) => {
            tracing::warn!(?err, ?git_root, "list_workspace_files failed");
            return Vec::new();
        },
    };
    files
        .into_iter()
        .filter_map(|path| parse_entry(&path, current_uid, fs))
        .collect()
}

fn parse_entry(
    path: &Path,
    current_uid: WorkspaceUid,
    fs: &dyn FsHost,
) -> Option<WorkspacePickerEntry> {
    let mut buf = Vec::new();
    if let Err(err) = fs.read(path, &mut buf) {
        tracing::info!(?err, ?path, "skipping unreadable workspace state file");
        return None;
    }
    let body = match String::from_utf8(buf) {
        Ok(body) => body,
        Err(err) => {
            tracing::info!(?err, ?path, "skipping non-utf8 workspace state file");
            return None;
        },
    };
    let state: crate::workspace_persist::WorkspaceStateV1 = match ron::from_str(&body) {
        Ok(state) => state,
        Err(err) => {
            tracing::info!(?err, ?path, "skipping unparseable workspace state file");
            return None;
        },
    };
    let label = if state.name.is_empty() {
        state
            .git_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(unnamed)")
            .to_string()
    } else {
        state.name
    };
    Some(WorkspacePickerEntry {
        path: path.to_path_buf(),
        label,
        is_current: state.uid == current_uid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{Entity, TestAppContext, VisualTestContext};
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

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        fs: Arc<FakeFs>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(
        cx: &'a mut TestAppContext,
        fake_fs: Arc<FakeFs>,
        git_root: &Path,
    ) -> Harness<'a> {
        install_globals(cx, fake_fs.clone());
        let git_root = git_root.to_path_buf();
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", git_root.clone(), cx));
        Harness {
            workspace,
            fs: fake_fs,
            vcx,
        }
    }

    fn write_state(fs: &FakeFs, path: &Path, state: &crate::workspace_persist::WorkspaceStateV1) {
        let body = ron::ser::to_string_pretty(state, ron::ser::PrettyConfig::default())
            .expect("serialize");
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent).expect("create state dir");
        }
        fs.write(path, body.as_bytes()).expect("write state file");
    }

    fn state_for(
        h: &mut Harness<'_>,
        uid: WorkspaceUid,
        name: &str,
        git_root: &Path,
    ) -> crate::workspace_persist::WorkspaceStateV1 {
        let mut state = h.workspace.read_with(h.vcx, |w, cx| w.to_state(cx));
        state.uid = uid;
        state.name = name.to_string();
        state.git_root = git_root.to_path_buf();
        state
    }

    #[test]
    fn open_workspace_picker_is_noop_when_no_state_exists() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let h = new_harness(&mut cx, fake_fs, Path::new("/repo"));

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<WorkspacePickerDelegate>>()
                .is_some()
        });
        assert!(!has_modal, "picker should not open with no state files");
    }

    #[test]
    fn collect_entries_lists_every_parseable_file() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let mut h = new_harness(&mut cx, fake_fs.clone(), Path::new("/repo"));

        let current_uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        let other_uid = WorkspaceUid(current_uid.0.wrapping_add(1));
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/repo"), &*fake_fs)
            .expect("dir");
        write_state(
            &fake_fs,
            &dir.join(format!("{current_uid}.ron")),
            &state_for(&mut h, current_uid, "alpha", Path::new("/repo")),
        );
        write_state(
            &fake_fs,
            &dir.join(format!("{other_uid}.ron")),
            &state_for(&mut h, other_uid, "beta", Path::new("/repo")),
        );

        let fs_dyn: Arc<dyn FsHost> = h.fs.clone();
        let entries = collect_entries(Path::new("/repo"), current_uid, &*fs_dyn);
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"alpha"));
        assert!(labels.contains(&"beta"));

        let current_count = entries.iter().filter(|e| e.is_current).count();
        assert_eq!(current_count, 1);
    }

    #[test]
    fn collect_entries_skips_unparseable_file() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let mut h = new_harness(&mut cx, fake_fs.clone(), Path::new("/repo"));

        let uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/repo"), &*fake_fs)
            .expect("dir");
        write_state(
            &fake_fs,
            &dir.join(format!("{uid}.ron")),
            &state_for(&mut h, uid, "alpha", Path::new("/repo")),
        );
        let bad = dir.join("garbage.ron");
        fake_fs.create_dir_all(&dir).expect("dir");
        fake_fs.write(&bad, b"not a valid ron").expect("write");

        let fs_dyn: Arc<dyn FsHost> = h.fs.clone();
        let entries = collect_entries(Path::new("/repo"), uid, &*fs_dyn);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "alpha");
    }

    #[test]
    fn refilter_narrows_against_label() {
        let entries = vec![
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/a.ron"),
                label: "alpha".to_string(),
                is_current: false,
            },
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/b.ron"),
                label: "beta".to_string(),
                is_current: false,
            },
        ];
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let mut delegate = WorkspacePickerDelegate::new(WeakEntity::new_invalid(), fs, entries);
        delegate.query = "alpha".to_string();
        delegate.refilter();
        assert_eq!(delegate.matches.len(), 1);
        assert_eq!(delegate.entries[delegate.matches[0].0].label, "alpha");
    }

    #[test]
    fn empty_query_lists_every_entry() {
        let entries = vec![
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/a.ron"),
                label: "alpha".to_string(),
                is_current: true,
            },
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/b.ron"),
                label: "beta".to_string(),
                is_current: false,
            },
        ];
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let delegate = WorkspacePickerDelegate::new(WeakEntity::new_invalid(), fs, entries);
        assert_eq!(delegate.match_count(), 2);
        assert_eq!(delegate.selected_index(), 0);
    }

    #[test]
    fn new_selects_current_workspace_entry() {
        let entries = vec![
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/a.ron"),
                label: "alpha".to_string(),
                is_current: false,
            },
            WorkspacePickerEntry {
                path: PathBuf::from("/dir/b.ron"),
                label: "beta".to_string(),
                is_current: true,
            },
        ];
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let delegate = WorkspacePickerDelegate::new(WeakEntity::new_invalid(), fs, entries);
        assert_eq!(delegate.selected_index(), 1);
    }

    #[test]
    fn render_row_marks_current_entry() {
        let entry = WorkspacePickerEntry {
            path: PathBuf::from("/dir/a.ron"),
            label: "alpha".to_string(),
            is_current: true,
        };
        assert_eq!(render_row(&entry), "alpha (current)");
    }

    #[test]
    fn open_workspace_picker_opens_modal_when_state_exists() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let mut h = new_harness(&mut cx, fake_fs.clone(), Path::new("/repo"));

        let uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/repo"), &*fake_fs)
            .expect("dir");
        write_state(
            &fake_fs,
            &dir.join(format!("{uid}.ron")),
            &state_for(&mut h, uid, "alpha", Path::new("/repo")),
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<WorkspacePickerDelegate>>()
            })
            .expect("workspace picker modal should be open");
        let labels = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| e.label.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(labels, vec!["alpha"]);
    }

    #[test]
    fn confirm_other_entry_calls_restore_state() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let mut h = new_harness(&mut cx, fake_fs.clone(), Path::new("/repo"));

        let current_uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        let other_uid = WorkspaceUid(current_uid.0.wrapping_add(7));
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/repo"), &*fake_fs)
            .expect("dir");
        write_state(
            &fake_fs,
            &dir.join(format!("{current_uid}.ron")),
            &state_for(&mut h, current_uid, "alpha", Path::new("/repo")),
        );
        write_state(
            &fake_fs,
            &dir.join(format!("{other_uid}.ron")),
            &state_for(&mut h, other_uid, "beta", Path::new("/repo")),
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<WorkspacePickerDelegate>>()
            })
            .expect("picker open");

        let other_idx = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .position(|e| e.label == "beta")
                .expect("other entry present")
        });
        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().set_selected_index(other_idx, cx)
        });
        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx)
        });
        h.vcx.run_until_parked();

        let restored_uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        assert_eq!(restored_uid, other_uid);
    }

    #[test]
    fn confirm_current_entry_is_noop_dismiss() {
        let mut cx = TestAppContext::single();
        let fake_fs = Arc::new(FakeFs::new());
        let mut h = new_harness(&mut cx, fake_fs.clone(), Path::new("/repo"));

        let current_uid = h.workspace.read_with(h.vcx, |w, _| w.uid());
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/repo"), &*fake_fs)
            .expect("dir");
        write_state(
            &fake_fs,
            &dir.join(format!("{current_uid}.ron")),
            &state_for(&mut h, current_uid, "alpha", Path::new("/repo")),
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<WorkspacePickerDelegate>>()
            })
            .expect("picker open");

        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx)
        });
        h.vcx.run_until_parked();

        let uid_after = h.workspace.read_with(h.vcx, |w, _| w.uid());
        assert_eq!(uid_after, current_uid);
    }
}
