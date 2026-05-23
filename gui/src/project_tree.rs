//! Project file tree dock item.
//!
//! Lists the immediate contents of the workspace root via
//! [`FsHost::list_dir`] and renders them as a directories-first list,
//! hosted in a left [`crate::dock::Dock`] and toggled by the
//! `ToggleProjectTree` action. Directory expansion and in-tree
//! navigation are not wired yet; the tree currently shows the root
//! level only.

use crate::{
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::statusbar_text_color,
};
use gpui::{div, App, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::host::FsHost;

/// One row in the tree: a file or directory directly under the listed
/// directory.
struct TreeEntry {
    name: String,
    is_dir: bool,
}

pub struct ProjectTree {
    git_root: PathBuf,
    entries: Vec<TreeEntry>,
}

impl ProjectTree {
    /// Build a tree listing the immediate contents of `git_root`. An
    /// unreadable root yields an empty list rather than an error so the
    /// dock still renders.
    pub fn new(git_root: PathBuf, fs: Arc<dyn FsHost>, _cx: &mut Context<'_, Self>) -> Self {
        let entries = read_entries(fs.as_ref(), &git_root);
        Self { git_root, entries }
    }
}

impl Render for ProjectTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let color = statusbar_text_color(cx);
        let rows = self.entries.iter().map(|entry| {
            let label = if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            div()
                .px_2()
                .text_color(color)
                .child(SharedString::from(label))
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

/// List the immediate children of `dir`, directories first then files,
/// each group ordered alphabetically by name. Empty on any IO error.
fn read_entries(fs: &dyn FsHost, dir: &Path) -> Vec<TreeEntry> {
    let mut entries: Vec<TreeEntry> = match fs.list_dir(dir) {
        Ok(items) => items
            .into_iter()
            .map(|entry| TreeEntry {
                name: entry.name.to_string(),
                is_dir: entry.is_dir,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat::host::FakeFs;

    #[test]
    fn read_entries_lists_dirs_first_then_alphabetical() {
        let fs = FakeFs::new();
        fs.insert_dir("/repo");
        fs.insert_file("/repo/readme.md", "");
        fs.insert_file("/repo/a.txt", "");
        fs.insert_dir("/repo/src");

        let entries = read_entries(&fs, Path::new("/repo"));
        let listed: Vec<(&str, bool)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.is_dir))
            .collect();
        assert_eq!(
            listed,
            [("src", true), ("a.txt", false), ("readme.md", false)]
        );
    }

    #[test]
    fn read_entries_empty_for_unreadable_root() {
        let fs = FakeFs::new();
        assert!(read_entries(&fs, Path::new("/missing")).is_empty());
    }
}
