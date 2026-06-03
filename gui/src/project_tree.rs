//! Project file tree dock item.
//!
//! Walks the workspace root via [`FsHost::list_dir`] and renders a
//! directories-first, collapsible tree hosted in a left
//! [`crate::dock::Dock`]. The `ToggleProjectTree` action opens and
//! closes it; while open the workspace is in `project_tree` keymap
//! mode, which routes navigation actions (`ProjectTreeSelectNext`,
//! `ProjectTreeCollapse`, `ProjectTreeConfirm`, ...) here.
//!
//! Expansion is tracked as a set of expanded directory paths; the
//! flattened list of visible [`Row`]s is recomputed from the
//! filesystem and that set whenever it changes, so a refresh picks up
//! external changes while preserving which directories are open.

use crate::{
    file_icons,
    globals::GitHostGlobal,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::ActiveTheme,
};
use gpui::{
    div, px, white, App, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window,
};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::host::FsHost;

const INDENT_SPACES_PER_DEPTH: usize = 2;

/// Git-derived decoration for a tree entry, driving its name color and
/// strikethrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitDecoration {
    Added,
    Modified,
    Deleted,
    Ignored,
}

/// Classify a changed file from its HEAD presence and on-disk
/// existence: absent from HEAD means a new (added) file; present in
/// HEAD and on disk means modified; present in HEAD but gone from disk
/// means a deletion.
fn changed_decoration(in_head: bool, on_disk: bool) -> GitDecoration {
    match (in_head, on_disk) {
        (false, _) => GitDecoration::Added,
        (true, true) => GitDecoration::Modified,
        (true, false) => GitDecoration::Deleted,
    }
}

/// One visible row in the flattened tree: a file or directory at a
/// given nesting `depth` below the root.
struct Row {
    path: PathBuf,
    name: String,
    is_dir: bool,
    depth: usize,
    decoration: Option<GitDecoration>,
}

pub struct ProjectTree {
    git_root: PathBuf,
    fs: Arc<dyn FsHost>,
    expanded: BTreeSet<PathBuf>,
    rows: Vec<Row>,
    selected: usize,
    /// Decoration for each changed path, refreshed from the git host on
    /// open and on every `refresh`. Ignored-file decorations are not
    /// cached here; they are derived per entry in `rebuild`.
    git_status: HashMap<PathBuf, GitDecoration>,
}

impl ProjectTree {
    /// Build a tree listing the immediate contents of `git_root`. An
    /// unreadable root yields an empty list rather than an error so the
    /// dock still renders.
    pub fn new(git_root: PathBuf, fs: Arc<dyn FsHost>, cx: &mut Context<'_, Self>) -> Self {
        let mut tree = Self {
            git_root,
            fs,
            expanded: BTreeSet::new(),
            rows: Vec::new(),
            selected: 0,
            git_status: HashMap::new(),
        };
        tree.recompute_git_status(cx);
        tree.rebuild();
        tree
    }

    /// Re-read the git host's changed-file set into [`Self::git_status`].
    /// No-op when no git host is installed (e.g. fs-only tests) or the
    /// root is not a repository, leaving the tree undecorated.
    fn recompute_git_status(&mut self, cx: &App) {
        self.git_status.clear();
        let repo = cx
            .try_global::<GitHostGlobal>()
            .and_then(|g| g.0.discover(&self.git_root));
        let Some(repo) = repo else {
            return;
        };
        for cf in repo.changed_files() {
            let in_head = repo.head_content(&cf.path).is_some();
            let on_disk = self.fs.exists(&cf.path);
            self.git_status
                .insert(cf.path, changed_decoration(in_head, on_disk));
        }
    }

    /// Decoration for `path`: its git change status if any, else
    /// `Ignored` when the workspace ignore stack excludes it, else none.
    fn decoration_for(&self, path: &Path) -> Option<GitDecoration> {
        if let Some(deco) = self.git_status.get(path) {
            return Some(*deco);
        }
        if self.fs.is_ignored(&self.git_root, path) {
            return Some(GitDecoration::Ignored);
        }
        None
    }

    /// Move the selection one visible row down, stopping at the last
    /// row.
    pub fn select_next(&mut self, cx: &mut Context<'_, Self>) {
        let last = self.rows.len().saturating_sub(1);
        if self.selected < last {
            self.selected += 1;
            cx.notify();
        }
    }

    /// Move the selection one visible row up, stopping at the first
    /// row.
    pub fn select_prev(&mut self, cx: &mut Context<'_, Self>) {
        if self.selected > 0 {
            self.selected -= 1;
            cx.notify();
        }
    }

    /// Collapse the selected directory. No-op when the selected row is
    /// a file or an already-collapsed directory.
    pub fn collapse(&mut self, cx: &mut Context<'_, Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        if self.expanded.remove(&path) {
            self.rebuild();
            cx.notify();
        }
    }

    /// Expand the selected directory, listing its contents inline.
    /// No-op when the selected row is a file or an already-expanded
    /// directory.
    pub fn expand(&mut self, cx: &mut Context<'_, Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        if self.expanded.insert(path) {
            self.rebuild();
            cx.notify();
        }
    }

    /// Act on the selected row: toggle a directory's expansion, or
    /// return the file path for the caller to open. Returns `None`
    /// when the selection is a directory or empty.
    pub fn confirm(&mut self, cx: &mut Context<'_, Self>) -> Option<PathBuf> {
        let row = self.rows.get(self.selected)?;
        let path = row.path.clone();
        if !row.is_dir {
            return Some(path);
        }
        if self.expanded.contains(&path) {
            self.expanded.remove(&path);
        } else {
            self.expanded.insert(path);
        }
        self.rebuild();
        cx.notify();
        None
    }

    /// The selected row's path, display name, and whether it is a
    /// directory, or `None` when the tree is empty.
    pub fn selected_entry(&self) -> Option<(PathBuf, String, bool)> {
        self.rows
            .get(self.selected)
            .map(|row| (row.path.clone(), row.name.clone(), row.is_dir))
    }

    /// Re-read the directory contents and git status from disk,
    /// preserving the set of expanded directories and clamping the
    /// selection into range.
    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.recompute_git_status(cx);
        self.rebuild();
        cx.notify();
    }

    /// The expanded directory paths, sorted, for workspace
    /// persistence.
    pub(crate) fn expanded_paths(&self) -> Vec<PathBuf> {
        self.expanded.iter().cloned().collect()
    }

    /// Replace the expanded-directory set and recompute the visible
    /// rows. Used by workspace restore to re-apply a persisted
    /// expansion; paths that no longer name a directory are simply
    /// never spliced in by [`build_rows`].
    pub(crate) fn set_expanded(&mut self, expanded: Vec<PathBuf>) {
        self.expanded = expanded.into_iter().collect();
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut rows = build_rows(self.fs.as_ref(), &self.git_root, &self.expanded);
        for row in &mut rows {
            row.decoration = self.decoration_for(&row.path);
        }
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.min(last);
    }
}

impl Render for ProjectTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let color = theme.statusbar_text;
        let selected = self.selected;
        let rows = self.rows.iter().enumerate().map(|(ix, row)| {
            let indent = " ".repeat(row.depth * INDENT_SPACES_PER_DEPTH);
            let name = if row.is_dir {
                format!("{}/", row.name)
            } else {
                row.name.clone()
            };
            let icon_color = if row.is_dir {
                theme.muted_text
            } else {
                file_icons::color_for_path(&row.path, &theme)
            };
            let (name_color, struck) = match row.decoration {
                Some(GitDecoration::Added) => (theme.git_added, false),
                Some(GitDecoration::Modified) => (theme.git_modified, false),
                Some(GitDecoration::Deleted) => (theme.git_deleted, true),
                Some(GitDecoration::Ignored) => (theme.muted_text, true),
                None => (color, false),
            };
            let mut name_el = div().text_color(name_color).child(SharedString::from(name));
            if struck {
                name_el = name_el.line_through();
            }
            let mut el = div()
                .flex()
                .items_center()
                .px_2()
                .text_color(color)
                .child(SharedString::from(indent))
                .child(
                    div()
                        .mr(px(6.0))
                        .text_color(icon_color)
                        .child(file_icons::icon_for_path(&row.path, row.is_dir)),
                )
                .child(name_el);
            if ix == selected {
                el = el.bg(white().opacity(0.1));
            }
            el
        });
        div().flex().flex_col().size_full().children(rows)
    }
}

impl ItemView for ProjectTree {
    fn tab_label(&self, _cx: &App) -> SharedString {
        self.git_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Files".to_string())
            .into()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::ProjectTree
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized,
    {
        DeserializeSnafu {
            reason: "ProjectTree persistence is not yet implemented",
        }
        .fail()
    }
}

/// Flatten the tree under `root` into visible rows. Directories listed
/// in `expanded` have their contents spliced in inline at the next
/// depth; everything else lists only its top level. Each directory's
/// children are ordered directories-first then alphabetically.
fn build_rows(fs: &dyn FsHost, root: &Path, expanded: &BTreeSet<PathBuf>) -> Vec<Row> {
    let mut rows = Vec::new();
    push_entries(fs, root, 0, expanded, &mut rows);
    rows
}

fn push_entries(
    fs: &dyn FsHost,
    dir: &Path,
    depth: usize,
    expanded: &BTreeSet<PathBuf>,
    rows: &mut Vec<Row>,
) {
    for (name, is_dir) in read_entries(fs, dir) {
        let path = dir.join(&name);
        let expand_here = is_dir && expanded.contains(&path);
        rows.push(Row {
            path,
            name,
            is_dir,
            depth,
            decoration: None,
        });
        if expand_here {
            let child = rows.last().expect("row just pushed").path.clone();
            push_entries(fs, &child, depth + 1, expanded, rows);
        }
    }
}

/// List the immediate children of `dir` as `(name, is_dir)`,
/// directories first then files, each group ordered alphabetically by
/// name. Empty on any IO error.
fn read_entries(fs: &dyn FsHost, dir: &Path) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = match fs.list_dir(dir) {
        Ok(items) => items
            .into_iter()
            .map(|entry| (entry.name.to_string(), entry.is_dir))
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, TestAppContext};
    use stoat::host::FakeFs;

    fn sample_fs() -> Arc<FakeFs> {
        let fs = FakeFs::new();
        fs.insert_dir("/repo");
        fs.insert_file("/repo/readme.md", "");
        fs.insert_file("/repo/a.txt", "");
        fs.insert_dir("/repo/src");
        fs.insert_file("/repo/src/main.rs", "");
        fs.insert_dir("/repo/src/inner");
        fs.insert_file("/repo/src/inner/deep.rs", "");
        Arc::new(fs)
    }

    fn new_tree(cx: &mut TestAppContext, fs: Arc<dyn FsHost>) -> Entity<ProjectTree> {
        cx.update(|cx| cx.new(|cx| ProjectTree::new(PathBuf::from("/repo"), fs, cx)))
    }

    fn visible(cx: &TestAppContext, tree: &Entity<ProjectTree>) -> Vec<(PathBuf, usize)> {
        tree.read_with(cx, |t, _| {
            t.rows.iter().map(|r| (r.path.clone(), r.depth)).collect()
        })
    }

    #[test]
    fn read_entries_lists_dirs_first_then_alphabetical() {
        let fs = sample_fs();
        let listed = read_entries(fs.as_ref(), Path::new("/repo"));
        assert_eq!(
            listed,
            [
                ("src".to_string(), true),
                ("a.txt".to_string(), false),
                ("readme.md".to_string(), false),
            ]
        );
    }

    #[test]
    fn build_rows_root_only_when_nothing_expanded() {
        let fs = sample_fs();
        let rows = build_rows(fs.as_ref(), Path::new("/repo"), &BTreeSet::new());
        let listed: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| (r.path.to_str().expect("utf8 path"), r.depth))
            .collect();
        assert_eq!(
            listed,
            [("/repo/src", 0), ("/repo/a.txt", 0), ("/repo/readme.md", 0)]
        );
    }

    #[test]
    fn build_rows_splices_expanded_children_at_next_depth() {
        let fs = sample_fs();
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/repo/src"));
        expanded.insert(PathBuf::from("/repo/src/inner"));
        let rows = build_rows(fs.as_ref(), Path::new("/repo"), &expanded);
        let listed: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| (r.path.to_str().expect("utf8 path"), r.depth))
            .collect();
        assert_eq!(
            listed,
            [
                ("/repo/src", 0),
                ("/repo/src/inner", 1),
                ("/repo/src/inner/deep.rs", 2),
                ("/repo/src/main.rs", 1),
                ("/repo/a.txt", 0),
                ("/repo/readme.md", 0),
            ]
        );
    }

    #[test]
    fn read_entries_empty_for_unreadable_root() {
        let fs = FakeFs::new();
        assert!(read_entries(&fs, Path::new("/missing")).is_empty());
    }

    fn decorations(
        cx: &TestAppContext,
        tree: &Entity<ProjectTree>,
    ) -> Vec<(PathBuf, Option<GitDecoration>)> {
        tree.read_with(cx, |t, _| {
            t.rows
                .iter()
                .map(|r| (r.path.clone(), r.decoration))
                .collect()
        })
    }

    #[test]
    fn changed_decoration_classifies_by_head_and_disk() {
        assert_eq!(changed_decoration(false, true), GitDecoration::Added);
        assert_eq!(changed_decoration(false, false), GitDecoration::Added);
        assert_eq!(changed_decoration(true, true), GitDecoration::Modified);
        assert_eq!(changed_decoration(true, false), GitDecoration::Deleted);
    }

    #[test]
    fn git_status_decorates_added_modified_and_classifies_deletions() {
        use crate::globals::GitHostGlobal;
        use stoat::host::{FakeGit, GitHost};

        let fs = FakeFs::new();
        fs.insert_dir("/repo");
        let git = Arc::new(FakeGit::new());
        {
            let mut builder = git.add_repo("/repo").with_fs(&fs);
            builder.added("new.rs", "fresh\n");
            builder.modified("mod.rs", "old\n", "changed\n");
            builder.deleted("gone.rs", "was\n");
        }
        let fs: Arc<dyn FsHost> = Arc::new(fs);

        let cx = TestAppContext::single();
        cx.update(|cx| cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>)));
        let tree = cx.update(|cx| cx.new(|cx| ProjectTree::new(PathBuf::from("/repo"), fs, cx)));

        let decos = decorations(&cx, &tree);
        let by_name = |name: &str| {
            decos
                .iter()
                .find(|(p, _)| p.ends_with(name))
                .map(|(_, d)| *d)
        };
        assert_eq!(by_name("new.rs"), Some(Some(GitDecoration::Added)));
        assert_eq!(by_name("mod.rs"), Some(Some(GitDecoration::Modified)));
        assert_eq!(by_name("gone.rs"), None, "deleted file is not a tree row");

        let gone = tree.read_with(&cx, |t, _| {
            t.git_status.get(Path::new("/repo/gone.rs")).copied()
        });
        assert_eq!(gone, Some(GitDecoration::Deleted));
    }

    #[test]
    fn gitignored_entry_is_decorated_ignored() {
        let fs = FakeFs::new();
        fs.insert_dir("/repo");
        fs.insert_file("/repo/.gitignore", "ignored.txt\n");
        fs.insert_file("/repo/ignored.txt", "");
        fs.insert_file("/repo/keep.txt", "");
        let fs: Arc<dyn FsHost> = Arc::new(fs);

        let cx = TestAppContext::single();
        let tree = cx.update(|cx| cx.new(|cx| ProjectTree::new(PathBuf::from("/repo"), fs, cx)));

        let decos = decorations(&cx, &tree);
        let by_name = |name: &str| {
            decos
                .iter()
                .find(|(p, _)| p.ends_with(name))
                .map(|(_, d)| *d)
        };
        assert_eq!(by_name("ignored.txt"), Some(Some(GitDecoration::Ignored)));
        assert_eq!(by_name("keep.txt"), Some(None));
    }

    #[test]
    fn select_next_and_prev_clamp_at_bounds() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx, sample_fs());

        tree.update(&mut cx, |t, cx| {
            t.select_prev(cx);
        });
        assert_eq!(tree.read_with(&cx, |t, _| t.selected), 0);

        tree.update(&mut cx, |t, cx| {
            for _ in 0..10 {
                t.select_next(cx);
            }
        });
        assert_eq!(tree.read_with(&cx, |t, _| t.selected), 2);
    }

    #[test]
    fn expand_then_collapse_reshapes_rows() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx, sample_fs());

        tree.update(&mut cx, |t, cx| t.expand(cx));
        assert_eq!(
            visible(&cx, &tree),
            [
                (PathBuf::from("/repo/src"), 0),
                (PathBuf::from("/repo/src/inner"), 1),
                (PathBuf::from("/repo/src/main.rs"), 1),
                (PathBuf::from("/repo/a.txt"), 0),
                (PathBuf::from("/repo/readme.md"), 0),
            ]
        );

        tree.update(&mut cx, |t, cx| t.collapse(cx));
        assert_eq!(
            visible(&cx, &tree),
            [
                (PathBuf::from("/repo/src"), 0),
                (PathBuf::from("/repo/a.txt"), 0),
                (PathBuf::from("/repo/readme.md"), 0),
            ]
        );
    }

    #[test]
    fn confirm_on_directory_toggles_and_returns_none() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx, sample_fs());

        let opened = tree.update(&mut cx, |t, cx| t.confirm(cx));
        assert_eq!(opened, None);
        assert_eq!(tree.read_with(&cx, |t, _| t.rows.len()), 5);
    }

    #[test]
    fn confirm_on_file_returns_path_without_changing_rows() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx, sample_fs());

        tree.update(&mut cx, |t, cx| {
            t.select_next(cx);
        });
        let opened = tree.update(&mut cx, |t, cx| t.confirm(cx));
        assert_eq!(opened, Some(PathBuf::from("/repo/a.txt")));
        assert_eq!(tree.read_with(&cx, |t, _| t.rows.len()), 3);
    }

    #[test]
    fn refresh_picks_up_new_file_and_keeps_expansion() {
        let mut cx = TestAppContext::single();
        let fs = sample_fs();
        let tree = new_tree(&mut cx, fs.clone());

        tree.update(&mut cx, |t, cx| t.expand(cx));
        fs.insert_file("/repo/src/added.rs", "");

        tree.update(&mut cx, |t, cx| t.refresh(cx));
        assert_eq!(
            visible(&cx, &tree),
            [
                (PathBuf::from("/repo/src"), 0),
                (PathBuf::from("/repo/src/inner"), 1),
                (PathBuf::from("/repo/src/added.rs"), 1),
                (PathBuf::from("/repo/src/main.rs"), 1),
                (PathBuf::from("/repo/a.txt"), 0),
                (PathBuf::from("/repo/readme.md"), 0),
            ]
        );
    }

    #[test]
    fn set_expanded_reshapes_rows_and_round_trips() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx, sample_fs());

        tree.update(&mut cx, |t, _| {
            t.set_expanded(vec![PathBuf::from("/repo/src")])
        });

        assert_eq!(
            tree.read_with(&cx, |t, _| t.expanded_paths()),
            vec![PathBuf::from("/repo/src")]
        );
        assert_eq!(
            visible(&cx, &tree),
            [
                (PathBuf::from("/repo/src"), 0),
                (PathBuf::from("/repo/src/inner"), 1),
                (PathBuf::from("/repo/src/main.rs"), 1),
                (PathBuf::from("/repo/a.txt"), 0),
                (PathBuf::from("/repo/readme.md"), 0),
            ]
        );
    }
}
