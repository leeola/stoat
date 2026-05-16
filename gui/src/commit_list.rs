use crate::{
    item::{DeserializeSnafu, ItemError, ItemView},
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
};
use gpui::{
    div, AnyElement, App, AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Task, Window,
};
use serde_json::Value;
use stoat::{
    commit_list::CommitListState as InnerCommitListState,
    host::{CommitFileChange, CommitInfo},
};
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// Entity-shaped wrapper around [`stoat::commit_list::CommitListState`].
/// Holds the underlying state and emits [`CommitListStateEvent`]s on
/// every mutation that affects rendering, so the picker and preview
/// pane can re-render without polling.
///
/// Mutations made through this wrapper's methods emit
/// [`CommitListStateEvent::Changed`]. Callers that mutate the inner
/// state through [`Self::inner_mut`] are responsible for emitting the
/// event themselves; for cross-state-machine mutations the dedicated
/// helpers ([`Self::set_selected`], [`Self::set_commits`]) emit on the
/// caller's behalf.
pub struct CommitListState {
    inner: InnerCommitListState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitListStateEvent {
    /// Catch-all signal that the inner state changed: selection moved,
    /// commits were appended, preview cache was populated, etc.
    Changed,
}

impl EventEmitter<CommitListStateEvent> for CommitListState {}

impl CommitListState {
    pub fn new(inner: InnerCommitListState) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &InnerCommitListState {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut InnerCommitListState {
        &mut self.inner
    }

    /// Move the cursor to `ix` and notify subscribers. No-op when the
    /// cursor is already at `ix`.
    pub fn set_selected(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.inner.selected == ix {
            return;
        }
        self.inner.selected = ix;
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }

    /// Replace the loaded commit page and notify subscribers. Used by
    /// tests and by future pagination wiring to push a fresh batch of
    /// commits into the picker's delegate via the state subscription.
    pub fn set_commits(&mut self, commits: Vec<CommitInfo>, cx: &mut Context<'_, Self>) {
        self.inner.commits = commits;
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }
}

/// Pane-hosted commit-list surface. Wraps an [`Entity<CommitListState>`]
/// and a [`Picker<CommitListDelegate>`]; renders the list on the left
/// and a per-commit file-change preview on the right.
///
/// The state subscription pulls fresh `(match_count, selected_index)`
/// into the delegate on every [`CommitListStateEvent::Changed`]. The
/// delegate caches these values because [`PickerDelegate::match_count`]
/// and [`PickerDelegate::selected_index`] take `&self` without a
/// context, so they cannot read from `Entity<...>` storage directly.
pub struct CommitListItem {
    state: Entity<CommitListState>,
    picker: Entity<Picker<CommitListDelegate>>,
    _state_subscription: Subscription,
}

impl CommitListItem {
    pub fn new(
        state: Entity<CommitListState>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let delegate = CommitListDelegate::new(state.clone(), cx);
        let picker = cx.new(|cx| Picker::new(delegate, window, cx));
        let subscription = {
            let picker = picker.clone();
            cx.subscribe(
                &state,
                move |_this, state, _event: &CommitListStateEvent, cx| {
                    let (count, selected) = {
                        let s = state.read(cx);
                        (s.inner.commits.len(), s.inner.selected)
                    };
                    picker.update(cx, |p, _cx| {
                        let delegate = p.delegate_mut();
                        delegate.count = count;
                        delegate.selected = selected;
                    });
                    cx.notify();
                },
            )
        };
        Self {
            state,
            picker,
            _state_subscription: subscription,
        }
    }

    pub fn state(&self) -> &Entity<CommitListState> {
        &self.state
    }

    pub fn picker(&self) -> &Entity<Picker<CommitListDelegate>> {
        &self.picker
    }
}

/// Picker delegate driven by [`Entity<CommitListState>`]. Reads commit
/// rows out of the shared state for `render_match` and mirrors cursor
/// movement back into the state so the preview pane, future action
/// handlers, and (eventually) workspace persistence all observe the
/// same `selected` index.
///
/// `count` and `selected` are cached because the [`PickerDelegate`]
/// trait's `match_count(&self)` / `selected_index(&self)` methods do
/// not receive a context. The owning [`CommitListItem`] keeps them in
/// sync with the inner state via a subscription on
/// [`CommitListStateEvent::Changed`].
pub struct CommitListDelegate {
    state: Entity<CommitListState>,
    count: usize,
    selected: usize,
}

impl CommitListDelegate {
    pub fn new(state: Entity<CommitListState>, cx: &App) -> Self {
        let (count, selected) = {
            let s = state.read(cx);
            (s.inner.commits.len(), s.inner.selected)
        };
        Self {
            state,
            count,
            selected,
        }
    }
}

impl PickerDelegate for CommitListDelegate {
    fn match_count(&self) -> usize {
        self.count
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<'_, Picker<Self>>) {
        if ix >= self.count {
            return;
        }
        self.selected = ix;
        self.state.update(cx, |s, cx| s.set_selected(ix, cx));
    }

    fn update_matches(&mut self, _query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        // FIXME: paginated walk against GitRepo::log_commits lands in
        // TODO.md "Review pipeline: commit list ItemView" line 19.
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _cx: &mut Context<'_, Picker<Self>>,
    ) {
        // FIXME: CommitsOpenReview handler is wired in TODO.md line 21.
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {
        // FIXME: CloseCommits handler is wired in TODO.md line 21.
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let row_text = {
            let s = self.state.read(cx);
            s.inner
                .commits
                .get(ix)
                .map(format_commit_row)
                .unwrap_or_default()
        };
        let color = statusbar_text_color(cx);
        let mut row = div().px_2().text_color(color).child(row_text);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

impl Render for CommitListItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (summary, has_session) = {
            let s = self.state.read(cx);
            let sha = s.inner.selected_sha().map(String::from);
            let summary = sha
                .as_deref()
                .and_then(|sha| s.inner.summaries.get(sha))
                .cloned();
            let has_session = sha
                .as_deref()
                .map(|sha| s.inner.preview_sessions.contains_key(sha))
                .unwrap_or(false);
            (summary, has_session)
        };

        div()
            .flex()
            .flex_row()
            .size_full()
            .child(div().flex_1().child(self.picker.clone()))
            .child(
                div()
                    .flex_1()
                    .child(render_preview_pane(summary.as_deref(), has_session)),
            )
    }
}

impl ItemView for CommitListItem {
    fn tab_label(&self, _cx: &App) -> SharedString {
        SharedString::from("Commits")
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        // FIXME: deserialize requires workspace-persistence wiring per
        // TODO.md "Workspace persistence" group; landing here once the
        // commit-list persistence shape (selected sha + scroll) is settled.
        DeserializeSnafu {
            reason: "CommitListItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }
}

fn render_preview_pane(
    summary: Option<&[CommitFileChange]>,
    has_session: bool,
) -> impl IntoElement {
    let mut col = div().flex().flex_col().size_full();
    match summary {
        Some(changes) if !changes.is_empty() => {
            for c in changes {
                col = col.child(format!(
                    "{}  +{} -{}",
                    c.rel_path.display(),
                    c.additions,
                    c.deletions,
                ));
            }
        },
        _ => {
            col = col.child("(no file change summary)");
        },
    }
    let footer = if has_session {
        "(preview loaded)"
    } else {
        "(no preview session)"
    };
    col.child(footer)
}

fn format_commit_row(commit: &CommitInfo) -> String {
    let author_first = commit.author_name.split_whitespace().next().unwrap_or("");
    let date = OffsetDateTime::from_unix_timestamp(commit.time)
        .ok()
        .and_then(|dt| dt.format(&DATE_FORMAT).ok())
        .unwrap_or_default();
    format!(
        "{}  {}  {}  {}",
        commit.short_sha, commit.summary, author_first, date,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn mk_commit(sha: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: format!("commit {sha}"),
            author_name: "Alice Author".to_string(),
            author_email: "alice@example.com".to_string(),
            time: 1_700_000_000,
            parent_count: 1,
        }
    }

    struct Harness<'a> {
        item: Entity<CommitListItem>,
        state: Entity<CommitListState>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_item(cx: &mut TestAppContext, commits: Vec<CommitInfo>) -> Harness<'_> {
        install_executor(cx);
        let state = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerCommitListState::new(PathBuf::from("/repo"));
                inner.commits = commits;
                CommitListState::new(inner)
            })
        });
        let vcx = cx.add_empty_window();
        let item = vcx.update(|window, cx| {
            let state = state.clone();
            cx.new(|cx| CommitListItem::new(state, window, cx))
        });
        Harness { item, state, vcx }
    }

    #[test]
    fn tab_label_returns_commits() {
        let mut cx = TestAppContext::single();
        let h = new_item(&mut cx, Vec::new());
        h.item.read_with(h.vcx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Commits"));
        });
    }

    #[test]
    fn is_dirty_is_false() {
        let mut cx = TestAppContext::single();
        let h = new_item(&mut cx, Vec::new());
        h.item.read_with(h.vcx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error() {
        let mut cx = TestAppContext::single();
        let h = new_item(&mut cx, Vec::new());
        let err = h.item.update(h.vcx, |_, cx| {
            CommitListItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn match_count_reflects_state_commits() {
        let mut cx = TestAppContext::single();
        let h = new_item(
            &mut cx,
            vec![mk_commit("c1"), mk_commit("c2"), mk_commit("c3")],
        );
        let count = h.item.read_with(h.vcx, |item, cx| {
            item.picker.read(cx).delegate().match_count()
        });
        assert_eq!(count, 3);
    }

    #[test]
    fn selected_index_reflects_state_selected() {
        let mut cx = TestAppContext::single();
        let h = new_item(&mut cx, vec![mk_commit("c1"), mk_commit("c2")]);
        let selected = h.item.read_with(h.vcx, |item, cx| {
            item.picker.read(cx).delegate().selected_index()
        });
        assert_eq!(selected, 0);
    }

    #[test]
    fn set_selected_index_propagates_to_state() {
        let mut cx = TestAppContext::single();
        let h = new_item(
            &mut cx,
            vec![mk_commit("c1"), mk_commit("c2"), mk_commit("c3")],
        );
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| p.set_selected_index(2, cx));
        h.vcx.run_until_parked();

        let state_selected = h.state.read_with(h.vcx, |s, _| s.inner.selected);
        let delegate_selected = picker.read_with(h.vcx, |p, _| p.delegate().selected_index());
        assert_eq!((state_selected, delegate_selected), (2, 2));
    }

    #[test]
    fn state_change_updates_delegate_cache() {
        let mut cx = TestAppContext::single();
        let h = new_item(&mut cx, Vec::new());
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        let initial = picker.read_with(h.vcx, |p, _| p.delegate().match_count());
        assert_eq!(initial, 0);

        h.state.update(h.vcx, |s, cx| {
            s.set_commits(vec![mk_commit("c1"), mk_commit("c2")], cx);
        });
        h.vcx.run_until_parked();

        let after = picker.read_with(h.vcx, |p, _| p.delegate().match_count());
        assert_eq!(after, 2);
    }
}
