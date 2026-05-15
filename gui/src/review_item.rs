use crate::{
    editor::Editor,
    item::{DeserializeSnafu, ItemError, ItemView},
    multi_buffer::MultiBuffer,
    review_session::ReviewSession,
};
use gpui::{div, App, Context, Entity, IntoElement, Render, SharedString, Styled, Window};
use serde_json::Value;
use stoat::review_session::ReviewSource;

/// Pane-hosted review surface. Wraps an [`Entity<ReviewSession>`] and
/// one [`ReviewFileView`] per file in the session; subsequent items
/// own MultiBuffer / Editor construction and the render path that
/// stacks each file's editor vertically.
pub struct ReviewItem {
    session: Entity<ReviewSession>,
    files: Vec<ReviewFileView>,
}

/// One file's view state: the workspace-relative path, the editor
/// over the file's review excerpts, and the underlying multi-buffer
/// that holds those excerpts.
pub struct ReviewFileView {
    pub rel_path: String,
    pub editor: Entity<Editor>,
    pub multi_buffer: Entity<MultiBuffer>,
}

impl ReviewItem {
    pub fn new(session: Entity<ReviewSession>, files: Vec<ReviewFileView>) -> Self {
        Self { session, files }
    }

    pub fn session(&self) -> &Entity<ReviewSession> {
        &self.session
    }

    pub fn files(&self) -> &[ReviewFileView] {
        &self.files
    }
}

impl Render for ReviewItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}

impl ItemView for ReviewItem {
    fn tab_label(&self, cx: &App) -> SharedString {
        review_source_label(&self.session.read(cx).inner().source).into()
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "ReviewItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }
}

fn review_source_label(source: &ReviewSource) -> String {
    match source {
        ReviewSource::WorkingTree { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Review: {name}")
        },
        ReviewSource::Commit { sha, .. } => format!("Commit: {}", short_sha(sha)),
        ReviewSource::CommitRange { from, to, .. } => {
            format!("Range: {}..{}", short_sha(from), short_sha(to))
        },
        ReviewSource::AgentEdits { .. } => String::from("Agent edits"),
        ReviewSource::InMemory { .. } => String::from("Review: in-memory"),
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::review_session::ReviewSession as InnerSession;

    fn new_item(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewItem> {
        cx.update(|cx| {
            let session = cx.new(|_| ReviewSession::new(InnerSession::new(source)));
            cx.new(|_| ReviewItem::new(session, Vec::new()))
        })
    }

    #[test]
    fn tab_label_reflects_working_tree_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repos/stoat"),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: stoat"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abcdef1234567890".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Commit: abcdef1"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_range_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::CommitRange {
                workdir: PathBuf::from("/repos/stoat"),
                from: "1111111aaaa".to_string(),
                to: "2222222bbbb".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.tab_label(app),
                SharedString::from("Range: 1111111..2222222")
            );
        });
    }

    #[test]
    fn tab_label_reflects_agent_edits_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::AgentEdits {
                edits: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Agent edits"));
        });
    }

    #[test]
    fn tab_label_reflects_in_memory_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: in-memory"));
        });
    }

    #[test]
    fn is_dirty_is_false() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        let err = item.update(&mut cx, |_, cx| {
            ReviewItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }
}
