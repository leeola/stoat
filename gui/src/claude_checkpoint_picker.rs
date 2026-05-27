//! Claude per-message checkpoint picker delegate.
//!
//! Lists every user message in the focused chat whose
//! `checkpoint_sha` was captured at submit time. Confirm routes the
//! selected sha through [`Workspace::restore_to_checkpoint`], which
//! rolls the workspace's git working tree back to the captured state
//! via [`stoat::host::GitRepo::restore_tree`]. TUI prior art lives at
//! `stoat/src/claude_checkpoint_picker.rs`; this implements the same
//! filter and ordering on top of the shared `Picker` primitive.

use crate::{
    claude_chat::focused_chat,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use stoat::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};

const LABEL_MAX_CHARS: usize = 80;

#[derive(Clone, Debug)]
pub struct ClaudeCheckpointEntry {
    pub sha: String,
    pub label: String,
}

pub struct ClaudeCheckpointPickerDelegate {
    workspace: WeakEntity<Workspace>,
    entries: Vec<ClaudeCheckpointEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl ClaudeCheckpointPickerDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, entries: Vec<ClaudeCheckpointEntry>) -> Self {
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        let selected = entries.len().saturating_sub(1);
        Self {
            workspace,
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
            .map(|(i, entry)| (i, entry.label.clone()));
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

    fn selected_sha(&self) -> Option<String> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx).map(|e| e.sha.clone())
    }
}

impl PickerDelegate for ClaudeCheckpointPickerDelegate {
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
        let Some(sha) = self.selected_sha() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            // Defer past the keystroke observer's outer `Workspace::update`
            // lease so the re-entrant update does not panic.
            window.defer(cx, move |_window, cx| {
                workspace.update(cx, |ws, cx| ws.restore_to_checkpoint(sha, cx));
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
        let color = cx.theme().statusbar_text;
        let runs = match_highlight_runs(
            &entry.label,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(entry.label.clone())).with_highlights(runs);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

/// Collect every restorable checkpoint from `messages` in
/// chronological order. Only user-role text messages whose
/// `checkpoint_sha` is set qualify; labels are trimmed and
/// truncated to [`LABEL_MAX_CHARS`] characters.
pub fn entries_from_messages(messages: &[ChatMessage]) -> Vec<ClaudeCheckpointEntry> {
    messages
        .iter()
        .filter_map(|msg| {
            if !matches!(msg.role, ChatRole::User) {
                return None;
            }
            let sha = msg.checkpoint_sha.clone()?;
            let text = match &msg.content {
                ChatMessageContent::Text(t) => t.as_str(),
                _ => return None,
            };
            let label: String = text.trim().chars().take(LABEL_MAX_CHARS).collect();
            Some(ClaudeCheckpointEntry { sha, label })
        })
        .collect()
}

/// Open the checkpoint picker over the focused Claude chat in
/// `workspace`. Silent no-op when no chat is focused or the chat
/// carries no restorable checkpoints.
pub fn open_claude_checkpoint_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(chat) = focused_chat(workspace, cx) else {
        return;
    };
    let entries = chat.read_with(cx, |c, _| entries_from_messages(&c.messages));
    if entries.is_empty() {
        return;
    }
    let weak_workspace = cx.weak_entity();
    workspace.toggle_modal::<Picker<ClaudeCheckpointPickerDelegate>, _>(
        window,
        cx,
        move |window, cx| {
            let delegate = ClaudeCheckpointPickerDelegate::new(weak_workspace, entries);
            Picker::new(delegate, window, cx)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        claude_chat::{dispatch_open_claude, ClaudeChat},
        globals::{
            ClaudeCodeHostGlobal, ClipboardHostGlobal, ExecutorGlobal, FsHostGlobal,
            FsWatchHostGlobal, GitHostGlobal,
        },
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat::host::{
        fake::{FakeClaudeCodeHost, FakeClipboard, FakeFs, FakeGit},
        ClaudeCodeHost, ClipboardHost, FsHost, FsWatchHost, GitHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(
        cx: &mut TestAppContext,
        host: Arc<dyn ClaudeCodeHost>,
        git: Arc<FakeGit>,
        fs: Arc<FakeFs>,
    ) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let clipboard: Arc<dyn ClipboardHost> = Arc::new(FakeClipboard::new());
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(ClaudeCodeHostGlobal(host));
            cx.set_global(ClipboardHostGlobal(clipboard));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        git: Arc<FakeGit>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, repo_root: &str) -> Harness<'a> {
        let host: Arc<dyn ClaudeCodeHost> = Arc::new(FakeClaudeCodeHost::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo(repo_root);
        let fs = Arc::new(FakeFs::new());
        install_globals(cx, host, git.clone(), fs);
        let root = PathBuf::from(repo_root);
        let (workspace, vcx) = cx.add_window_view({
            let root = root.clone();
            move |_window, cx| Workspace::new("main", root, cx)
        });
        Harness {
            workspace,
            git,
            vcx,
        }
    }

    fn user(text: &str, sha: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(text.to_string()),
            checkpoint_sha: sha.map(String::from),
        }
    }

    fn assistant_text(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: ChatMessageContent::Text(text.to_string()),
            checkpoint_sha: None,
        }
    }

    fn open_chat_with_messages(
        h: &mut Harness<'_>,
        messages: Vec<ChatMessage>,
    ) -> Entity<ClaudeChat> {
        h.workspace.update_in(h.vcx, |w, window, cx| {
            dispatch_open_claude(w, window, cx);
        });
        let chat = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                let pane_id = w.pane_tree().read(cx).focus();
                let pane = w.pane_tree().read(cx).pane(pane_id).cloned()?;
                let view = pane
                    .read(cx)
                    .active_item()
                    .map(crate::item::ItemHandle::to_any_view)?;
                view.downcast::<ClaudeChat>().ok()
            })
            .expect("chat is focused active item");
        chat.update(h.vcx, |c, _| c.messages = messages);
        chat
    }

    #[test]
    fn entries_from_messages_keeps_user_text_with_sha() {
        let messages = vec![
            user("first", Some("sha1")),
            assistant_text("response"),
            user("no checkpoint", None),
            user("second", Some("sha2")),
        ];
        let entries = entries_from_messages(&messages);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha, "sha1");
        assert_eq!(entries[0].label, "first");
        assert_eq!(entries[1].sha, "sha2");
        assert_eq!(entries[1].label, "second");
    }

    #[test]
    fn entries_skip_assistant_even_with_sha() {
        let mut a = assistant_text("oddly checkpointed");
        a.checkpoint_sha = Some("ignored".into());
        let entries = entries_from_messages(&[a]);
        assert!(entries.is_empty());
    }

    #[test]
    fn entries_label_truncates_to_max_chars() {
        let long = "x".repeat(LABEL_MAX_CHARS + 50);
        let entries = entries_from_messages(&[user(&long, Some("s"))]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label.chars().count(), LABEL_MAX_CHARS);
    }

    #[test]
    fn delegate_defaults_selection_to_last_entry() {
        let entries = vec![
            ClaudeCheckpointEntry {
                sha: "s1".into(),
                label: "first".into(),
            },
            ClaudeCheckpointEntry {
                sha: "s2".into(),
                label: "second".into(),
            },
            ClaudeCheckpointEntry {
                sha: "s3".into(),
                label: "third".into(),
            },
        ];
        let delegate = ClaudeCheckpointPickerDelegate::new(WeakEntity::new_invalid(), entries);
        assert_eq!(delegate.match_count(), 3);
        assert_eq!(delegate.selected_index(), 2);
    }

    #[test]
    fn refilter_narrows_against_label() {
        let entries = vec![
            ClaudeCheckpointEntry {
                sha: "s1".into(),
                label: "alpha first".into(),
            },
            ClaudeCheckpointEntry {
                sha: "s2".into(),
                label: "beta second".into(),
            },
        ];
        let mut delegate = ClaudeCheckpointPickerDelegate::new(WeakEntity::new_invalid(), entries);
        delegate.query = "beta".into();
        delegate.refilter();
        assert_eq!(delegate.matches.len(), 1);
        assert_eq!(delegate.entries[delegate.matches[0].0].sha, "s2");
    }

    #[test]
    fn open_picker_is_noop_when_no_focused_chat() {
        let mut cx = TestAppContext::single();
        let h = new_harness(&mut cx, "/repo");

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_claude_checkpoint_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<ClaudeCheckpointPickerDelegate>>()
                .is_some()
        });
        assert!(!has_modal, "picker should not open without a chat");
    }

    #[test]
    fn open_picker_is_noop_when_chat_has_no_checkpoints() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx, "/repo");
        let _chat = open_chat_with_messages(
            &mut h,
            vec![user("no checkpoint", None), assistant_text("reply")],
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_claude_checkpoint_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<ClaudeCheckpointPickerDelegate>>()
                .is_some()
        });
        assert!(!has_modal, "picker should not open without checkpoints");
    }

    #[test]
    fn open_picker_lists_chronological_entries() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx, "/repo");
        let _chat = open_chat_with_messages(
            &mut h,
            vec![
                user("first", Some("sha1")),
                user("second", Some("sha2")),
                user("third", Some("sha3")),
            ],
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_claude_checkpoint_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<ClaudeCheckpointPickerDelegate>>()
            })
            .expect("picker open");
        let (entries, selected) = picker.read_with(h.vcx, |p, _| {
            let entries = p
                .delegate()
                .entries
                .iter()
                .map(|e| (e.sha.clone(), e.label.clone()))
                .collect::<Vec<_>>();
            (entries, p.delegate().selected_index())
        });
        assert_eq!(
            entries,
            vec![
                ("sha1".into(), "first".into()),
                ("sha2".into(), "second".into()),
                ("sha3".into(), "third".into()),
            ],
        );
        assert_eq!(selected, 2);
    }

    #[test]
    fn confirm_routes_selected_sha_to_git_restore_tree() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx, "/repo");
        h.git
            .add_repo("/repo")
            .commit("sha1", &[])
            .commit("sha2", &[]);
        let _chat = open_chat_with_messages(
            &mut h,
            vec![user("first", Some("sha1")), user("second", Some("sha2"))],
        );

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_claude_checkpoint_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<ClaudeCheckpointPickerDelegate>>()
            })
            .expect("picker open");
        picker.update(h.vcx, |p, cx| p.delegate_mut().set_selected_index(0, cx));
        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx)
        });
        h.vcx.run_until_parked();

        assert_eq!(h.git.restored_shas(Path::new("/repo")), vec!["sha1"]);
    }
}
