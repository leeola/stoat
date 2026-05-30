//! Git status picker delegate.
//!
//! Lists every working-tree-changed path in the active git
//! repository alongside a status glyph (S/M/A for staged,
//! modified, or untracked). Fuzzy-filters against the query and
//! on confirm opens the selected path via
//! [`crate::workspace::Workspace::open_paths`].

use crate::{
    file_icons,
    globals::GitHostGlobal,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, px, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Staged,
    Modified,
    Untracked,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Staged => "S ",
            Status::Modified => "M ",
            Status::Untracked => "A ",
        }
    }

    fn sort_order(self) -> u8 {
        match self {
            Status::Staged => 0,
            Status::Modified => 1,
            Status::Untracked => 2,
        }
    }
}

pub struct GitStatusEntry {
    path: PathBuf,
    status: Status,
}

pub struct GitStatusDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    entries: Vec<GitStatusEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl GitStatusDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        git_root: PathBuf,
        mut entries: Vec<GitStatusEntry>,
    ) -> Self {
        entries.sort_by(|a, b| {
            a.status
                .sort_order()
                .cmp(&b.status.sort_order())
                .then_with(|| a.path.cmp(&b.path))
        });
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

impl PickerDelegate for GitStatusDelegate {
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
        let label_text = format!("{}{}", entry.status.glyph(), display);
        let prefix_len = entry.status.glyph().len();
        let shifted: Vec<u32> = matched.iter().map(|i| i + prefix_len as u32).collect();
        let theme = cx.theme();
        let color = theme.statusbar_text;
        let runs = match_highlight_runs(
            &label_text,
            &shifted,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(label_text)).with_highlights(runs);
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

/// Open the git status picker over the workspace's repository.
/// No-op when the workspace's git root cannot be discovered.
pub fn open_git_status_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let entries = collect_entries(&git_root, cx);
    workspace.toggle_modal::<Picker<GitStatusDelegate>, _>(window, cx, move |window, cx| {
        let delegate = GitStatusDelegate::new(weak_workspace, git_root, entries);
        Picker::new(delegate, window, cx)
    });
}

fn collect_entries(git_root: &Path, cx: &gpui::App) -> Vec<GitStatusEntry> {
    let git = cx.global::<GitHostGlobal>().0.clone();
    let Some(repo) = git.discover(git_root) else {
        return Vec::new();
    };
    repo.changed_files()
        .into_iter()
        .map(|cf| {
            let status = if cf.staged {
                Status::Staged
            } else if repo.head_content(&cf.path).is_some() {
                Status::Modified
            } else {
                Status::Untracked
            };
            GitStatusEntry {
                path: cf.path,
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, GitHostGlobal};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::{fake::FakeGit, FakeFs, FsWatchHost, GitHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, fs: Arc<FakeFs>, git: Arc<FakeGit>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
    }

    fn entry(status: Status, path: &str) -> GitStatusEntry {
        GitStatusEntry {
            path: PathBuf::from(path),
            status,
        }
    }

    fn new_delegate(git_root: &str, entries: Vec<GitStatusEntry>) -> GitStatusDelegate {
        GitStatusDelegate::new(WeakEntity::new_invalid(), PathBuf::from(git_root), entries)
    }

    fn match_labels(delegate: &GitStatusDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| {
                let e = &delegate.entries[*i];
                format!(
                    "{}{}",
                    e.status.glyph(),
                    display_path(&e.path, &delegate.git_root)
                )
            })
            .collect()
    }

    #[test]
    fn new_delegate_groups_entries_by_status_then_path() {
        let delegate = new_delegate(
            "/repo",
            vec![
                entry(Status::Untracked, "/repo/new.rs"),
                entry(Status::Modified, "/repo/src/main.rs"),
                entry(Status::Staged, "/repo/src/lib.rs"),
                entry(Status::Modified, "/repo/README"),
            ],
        );
        assert_eq!(
            match_labels(&delegate),
            vec!["S src/lib.rs", "M README", "M src/main.rs", "A new.rs",],
        );
    }

    #[test]
    fn refilter_narrows_against_query() {
        let mut delegate = new_delegate(
            "/repo",
            vec![
                entry(Status::Modified, "/repo/src/main.rs"),
                entry(Status::Untracked, "/repo/src/lib.rs"),
            ],
        );
        delegate.query = "main".to_string();
        delegate.refilter();
        assert_eq!(match_labels(&delegate), vec!["M src/main.rs"]);
    }

    #[test]
    fn no_entries_yields_empty_match_list() {
        let delegate = new_delegate("/repo", vec![]);
        assert_eq!(delegate.match_count(), 0);
    }

    #[test]
    fn collect_classifies_modified_untracked_and_staged() {
        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        {
            let mut builder = git.add_repo("/repo").with_fs(&fs);
            builder.modified("a.rs", "v1\n", "v2\n");
            builder.unstaged_file("new.rs", "fresh\n");
            builder.staged_file("staged.rs", "queued\n");
        }
        install_globals(&mut cx, fs, git);

        let mut entries = cx.update(|cx| collect_entries(Path::new("/repo"), cx));
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let by_name: Vec<(String, Status)> = entries
            .into_iter()
            .map(|e| {
                let name = e
                    .path
                    .file_name()
                    .expect("test fixture path has a file name")
                    .to_string_lossy()
                    .into_owned();
                (name, e.status)
            })
            .collect();
        assert_eq!(
            by_name,
            vec![
                ("a.rs".to_string(), Status::Modified),
                ("new.rs".to_string(), Status::Untracked),
                ("staged.rs".to_string(), Status::Staged),
            ],
        );
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(
        cx: &'a mut TestAppContext,
        fs: Arc<FakeFs>,
        git: Arc<FakeGit>,
    ) -> Harness<'a> {
        install_globals(cx, fs, git);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness { workspace, vcx }
    }

    #[test]
    fn open_git_status_picker_makes_picker_modal_active() {
        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo")
            .with_fs(&fs)
            .modified("a.rs", "v1\n", "v2\n");

        let h = new_harness(&mut cx, fs, git);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_git_status_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<GitStatusDelegate>>()
                .is_some()
        });
        assert!(active, "git status picker should be the active modal");
    }
}
