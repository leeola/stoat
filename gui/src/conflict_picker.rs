//! Conflict picker delegate.
//!
//! Lists every working-tree file with unresolved merge conflicts in
//! the active git repository. Fuzzy-filters against the query and on
//! confirm opens the selected path via
//! [`crate::workspace::Workspace::open_paths`], where the editor
//! surfaces the conflict regions for resolution.

use crate::{
    globals::GitHostGlobal,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::path::{Path, PathBuf};

pub struct ConflictDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    entries: Vec<PathBuf>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl ConflictDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        git_root: PathBuf,
        mut entries: Vec<PathBuf>,
    ) -> Self {
        entries.sort();
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

        let items = self.entries.iter().enumerate().map(|(i, path)| {
            let display = display_path(path, &self.git_root);
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
        self.entries.get(*idx).map(PathBuf::as_path)
    }
}

impl PickerDelegate for ConflictDelegate {
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

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(path) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let label_text = display_path(path, &self.git_root);
        let color = cx.theme().statusbar_text;
        let runs = match_highlight_runs(
            &label_text,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(label_text)).with_highlights(runs);
        div()
            .px_2()
            .text_color(color)
            .child(label)
            .into_any_element()
    }
}

fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Open the conflict picker over the workspace's repository. No-op when
/// the workspace's git root cannot be discovered or has no conflicts.
pub fn open_conflict_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let entries = collect_entries(&git_root, cx);
    workspace.toggle_modal::<Picker<ConflictDelegate>, _>(window, cx, move |window, cx| {
        let delegate = ConflictDelegate::new(weak_workspace, git_root, entries);
        Picker::new(delegate, window, cx)
    });
}

fn collect_entries(git_root: &Path, cx: &gpui::App) -> Vec<PathBuf> {
    let git = cx.global::<GitHostGlobal>().0.clone();
    let Some(repo) = git.discover(git_root) else {
        return Vec::new();
    };
    repo.conflicted_files()
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

    fn new_delegate(git_root: &str, entries: Vec<PathBuf>) -> ConflictDelegate {
        ConflictDelegate::new(WeakEntity::new_invalid(), PathBuf::from(git_root), entries)
    }

    fn match_labels(delegate: &ConflictDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| display_path(&delegate.entries[*i], &delegate.git_root))
            .collect()
    }

    #[test]
    fn new_delegate_sorts_entries_by_path() {
        let delegate = new_delegate(
            "/repo",
            vec![
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/README"),
                PathBuf::from("/repo/src/lib.rs"),
            ],
        );
        assert_eq!(
            match_labels(&delegate),
            vec!["README", "src/lib.rs", "src/main.rs"],
        );
    }

    #[test]
    fn refilter_narrows_against_query() {
        let mut delegate = new_delegate(
            "/repo",
            vec![
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/src/lib.rs"),
            ],
        );
        delegate.query = "main".to_string();
        delegate.refilter();
        assert_eq!(match_labels(&delegate), vec!["src/main.rs"]);
    }

    #[test]
    fn no_entries_yields_empty_match_list() {
        let delegate = new_delegate("/repo", vec![]);
        assert_eq!(delegate.match_count(), 0);
    }

    #[test]
    fn collect_lists_conflicted_files_in_order() {
        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        {
            let mut builder = git.add_repo("/repo");
            builder.conflicted_file("src/main.rs");
            builder.conflicted_file("README");
            builder.conflicted_file("src/lib.rs");
        }
        install_globals(&mut cx, fs, git);

        let entries = cx.update(|cx| collect_entries(Path::new("/repo"), cx));
        let labels: Vec<String> = entries
            .iter()
            .map(|p| display_path(p, Path::new("/repo")))
            .collect();
        assert_eq!(labels, vec!["README", "src/lib.rs", "src/main.rs"]);
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
    fn open_conflict_picker_makes_picker_modal_active() {
        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").conflicted_file("a.rs");

        let h = new_harness(&mut cx, fs, git);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_conflict_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<ConflictDelegate>>()
                .is_some()
        });
        assert!(active, "conflict picker should be the active modal");
    }
}
