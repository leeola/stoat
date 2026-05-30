//! Buffer picker delegate.
//!
//! Lists every path-bound buffer the workspace's
//! [`crate::buffer_registry::BufferRegistry`] currently tracks, fuzzy
//! filters against the query, and on confirm routes through
//! [`crate::workspace::Workspace::open_paths`] so the existing
//! [`stoat::buffer::SharedBuffer`] is reused (the inner registry's
//! `open` returns the existing entry for known paths). Scratch
//! buffers carry no path and are intentionally omitted.

use crate::{
    file_icons,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, px, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::path::{Path, PathBuf};
use stoat::buffer::BufferId;

pub struct BufferPickerDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    /// Snapshot of the open path-bound buffers at picker open time,
    /// sorted by path for a deterministic presentation. Scratch
    /// buffers (no path) are omitted.
    entries: Vec<BufferEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    /// Cached query string. Held here so the fuzzy filter can re-run
    /// without reading from the picker's editor when refilter is
    /// called outside of a query-change tick (none today, but the
    /// pattern matches the file finder for symmetry).
    query: String,
}

pub struct BufferEntry {
    #[allow(dead_code)]
    id: BufferId,
    path: PathBuf,
}

impl BufferPickerDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        git_root: PathBuf,
        mut entries: Vec<BufferEntry>,
    ) -> Self {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        Self {
            workspace,
            git_root,
            entries,
            matches,
            selected: 0,
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

        let items = self.entries.iter().enumerate().map(|(i, entry)| {
            let display = display_path(&entry.path, &self.git_root);
            (i, display)
        });
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

    fn selected_path(&self) -> Option<&Path> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx).map(|e| e.path.as_path())
    }
}

impl PickerDelegate for BufferPickerDelegate {
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
        let Some(path) = self.selected_path().map(Path::to_path_buf) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |ws, cx| ws.open_paths(&[path], cx));
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
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let display = display_path(&entry.path, &self.git_root);
        let theme = cx.theme();
        let color = theme.statusbar_text;
        let runs = match_highlight_runs(
            &display,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(display)).with_highlights(runs);
        let mut row = div()
            .flex()
            .items_center()
            .px_2()
            .child(
                div()
                    .mr(px(6.0))
                    .text_color(file_icons::color_for_path(&entry.path, &theme))
                    .child(file_icons::icon_for_path(&entry.path, false)),
            )
            .child(div().text_color(color).child(label));
        if selected {
            row = row.bg(theme.modal_selection);
        }
        row.into_any_element()
    }
}

fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Open the buffer picker over the workspace's open path-bound
/// buffer set. Scratch buffers are excluded -- the picker reopens
/// files by path, and a scratch buffer has none.
pub fn open_buffer_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let entries = collect_entries(workspace, cx);
    workspace.toggle_modal::<Picker<BufferPickerDelegate>, _>(window, cx, move |window, cx| {
        let delegate = BufferPickerDelegate::new(weak_workspace, git_root, entries);
        Picker::new(delegate, window, cx)
    });
}

fn collect_entries(workspace: &Workspace, cx: &gpui::App) -> Vec<BufferEntry> {
    let registry = workspace.buffer_registry().read(cx);
    registry
        .ids()
        .filter_map(|id| {
            registry.path_for(id).map(|p| BufferEntry {
                id,
                path: p.to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::{FakeFs, FsWatchHost};
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

    fn entry(id: u64, path: &str) -> BufferEntry {
        BufferEntry {
            id: BufferId::new(id),
            path: PathBuf::from(path),
        }
    }

    fn new_delegate(git_root: &str, entries: Vec<BufferEntry>) -> BufferPickerDelegate {
        BufferPickerDelegate::new(WeakEntity::new_invalid(), PathBuf::from(git_root), entries)
    }

    fn match_names(delegate: &BufferPickerDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| display_path(&delegate.entries[*i].path, &delegate.git_root))
            .collect()
    }

    #[test]
    fn new_delegate_sorts_entries_by_path() {
        let delegate = new_delegate(
            "/repo",
            vec![
                entry(1, "/repo/src/main.rs"),
                entry(2, "/repo/src/lib.rs"),
                entry(3, "/repo/README"),
            ],
        );
        assert_eq!(
            match_names(&delegate),
            vec!["README", "src/lib.rs", "src/main.rs"],
        );
    }

    #[test]
    fn refilter_narrows_against_query() {
        let mut delegate = new_delegate(
            "/repo",
            vec![
                entry(1, "/repo/src/main.rs"),
                entry(2, "/repo/src/lib.rs"),
                entry(3, "/repo/README"),
            ],
        );
        delegate.query = "main".to_string();
        delegate.refilter();
        assert_eq!(match_names(&delegate), vec!["src/main.rs"]);
    }

    #[test]
    fn empty_query_lists_every_entry() {
        let delegate = new_delegate(
            "/repo",
            vec![entry(1, "/repo/a.rs"), entry(2, "/repo/b.rs")],
        );
        assert_eq!(delegate.match_count(), 2);
    }

    #[test]
    fn no_entries_yields_empty_match_list() {
        let delegate = new_delegate("/repo", vec![]);
        assert_eq!(delegate.match_count(), 0);
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

    #[test]
    fn delegate_omits_scratch_buffers() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.workspace.update(h.vcx, |w, cx| {
            w.buffer_registry().update(cx, |r, cx| {
                let _ = r.new_scratch(cx);
            });
        });

        let entries = h.workspace.read_with(h.vcx, collect_entries);
        assert_eq!(entries.len(), 1, "scratch buffer should be omitted");
        assert_eq!(entries[0].path, PathBuf::from("/repo/a.rs"));
    }

    #[test]
    fn open_buffer_picker_makes_picker_modal_active() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_buffer_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<BufferPickerDelegate>>()
                .is_some()
        });
        assert!(active, "buffer picker should be the active modal");
    }
}
