use crate::{
    buffer_registry::BufferRegistry,
    globals::{ExecutorGlobal, GitHostGlobal, LanguageRegistry},
    item::{DeserializeSnafu, ItemError, ItemView},
    picker::{Picker, PickerDelegate, PickerSecondary},
    review_item::ReviewItem,
    review_session::ReviewSession as GuiReviewSession,
    theme::statusbar_text_color,
};
use gpui::{
    div, AnyElement, App, AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Task, Window,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    commit_list::CommitListState as InnerCommitListState,
    host::{CommitFileChange, CommitInfo, GitHost, GitRepo},
    review::ReviewFileInput,
    review_session::{ReviewSession as InnerReviewSession, ReviewSource},
};
use stoat_scheduler::Executor;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// Number of commits requested per pagination call. Matches the TUI's
/// `COMMITS_INITIAL_PAGE` so a fresh open lands the same first batch
/// in both surfaces.
const COMMITS_PAGE_LIMIT: usize = 64;

/// Prefetch window: when the cursor lands within this many rows of
/// the loaded tail, the delegate spawns the next page so navigation
/// stays ahead of the user.
const COMMITS_PREFETCH_GAP: usize = 8;

/// Page-step size for `CommitsPageDown` / `CommitsPageUp`. Matches
/// the TUI's `action_handlers::commits::COMMITS_PAGE_STEP` so the
/// page-down distance is the same in both surfaces.
pub const COMMITS_PAGE_STEP: usize = 16;

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

    /// Append a freshly loaded page of commits to the tail. An empty
    /// `page` flips `reached_end` because the underlying walker
    /// returns `[]` only when it cannot produce another commit after
    /// the last loaded sha (root commit or unknown anchor).
    pub fn append_page(&mut self, page: Vec<CommitInfo>, cx: &mut Context<'_, Self>) {
        if page.is_empty() {
            self.inner.reached_end = true;
        } else {
            self.inner.commits.extend(page);
        }
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }

    /// Cache the per-file change summary for `sha`. Used by the
    /// background preview pipeline so the right pane's file-stats
    /// list stays in sync with the loaded preview.
    pub fn set_summary(
        &mut self,
        sha: String,
        changes: Vec<CommitFileChange>,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.summaries.insert(sha, changes);
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }

    /// Cache the built review session for `sha`. The `Arc<inner>`
    /// shape mirrors `stoat::commit_list::CommitListState::preview_sessions`
    /// so renderers that share the inner state see the same cache.
    pub fn set_preview_session(
        &mut self,
        sha: String,
        session: Arc<stoat::review_session::ReviewSession>,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.preview_sessions.insert(sha, session);
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }

    /// Reset the commit / preview caches to their freshly opened
    /// state. Used by `CommitsRefresh` so the next page reload
    /// starts from HEAD without any stale rows.
    pub fn reset_caches(&mut self, cx: &mut Context<'_, Self>) {
        self.inner.commits.clear();
        self.inner.reached_end = false;
        self.inner.selected = 0;
        self.inner.scroll_top = 0;
        self.inner.summaries.clear();
        self.inner.preview_sessions.clear();
        cx.emit(CommitListStateEvent::Changed);
        cx.notify();
    }
}

/// Pane-hosted commit-list surface. Wraps an [`Entity<CommitListState>`]
/// and a [`Picker<CommitListDelegate>`]; renders the list on the left
/// and a per-commit file-change preview on the right.
///
/// The state subscription pulls fresh `(match_count, selected_index,
/// reached_end)` into the delegate on every
/// [`CommitListStateEvent::Changed`] and fans out a background
/// preview load for the newly selected sha when neither cached nor
/// already in flight. The delegate caches `count` / `selected_index`
/// because the [`PickerDelegate`] trait's accessors take `&self`
/// without a context.
///
/// Built previews land in [`Self::preview_items`] keyed by sha so
/// re-renders don't rebuild `Entity<Editor>` instances; the
/// rendered right pane embeds the corresponding entry directly.
pub struct CommitListItem {
    state: Entity<CommitListState>,
    picker: Entity<Picker<CommitListDelegate>>,
    buffer_registry: Entity<BufferRegistry>,
    workdir: PathBuf,
    git_host: Arc<dyn GitHost>,
    executor: Executor,
    loading_previews: HashSet<String>,
    preview_items: HashMap<String, Entity<ReviewItem>>,
    _state_subscription: Subscription,
}

impl CommitListItem {
    pub fn new(
        state: Entity<CommitListState>,
        buffer_registry: Entity<BufferRegistry>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let delegate = CommitListDelegate::new(state.clone(), cx);
        let picker = cx.new(|cx| Picker::new(delegate, window, cx));
        let workdir = state.read(cx).inner.workdir.clone();
        let git_host = cx.global::<GitHostGlobal>().0.clone();
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let subscription = {
            let picker = picker.clone();
            cx.subscribe(
                &state,
                move |this, state, _event: &CommitListStateEvent, cx| {
                    let (count, selected, reached_end, selected_sha) = {
                        let s = state.read(cx);
                        (
                            s.inner.commits.len(),
                            s.inner.selected,
                            s.inner.reached_end,
                            s.inner.selected_sha().map(String::from),
                        )
                    };
                    picker.update(cx, |p, _cx| {
                        let delegate = p.delegate_mut();
                        delegate.count = count;
                        delegate.selected = selected;
                        delegate.reached_end = reached_end;
                    });
                    if let Some(sha) = selected_sha {
                        this.start_preview_load(sha, cx);
                    }
                    cx.notify();
                },
            )
        };
        Self {
            state,
            picker,
            buffer_registry,
            workdir,
            git_host,
            executor,
            loading_previews: HashSet::new(),
            preview_items: HashMap::new(),
            _state_subscription: subscription,
        }
    }

    pub fn state(&self) -> &Entity<CommitListState> {
        &self.state
    }

    pub fn picker(&self) -> &Entity<Picker<CommitListDelegate>> {
        &self.picker
    }

    pub fn preview_items(&self) -> &HashMap<String, Entity<ReviewItem>> {
        &self.preview_items
    }

    /// Discard every cached commit, summary, preview entity, and
    /// in-flight load marker, then kick the picker's
    /// `update_matches` so the first page reloads from HEAD. Used
    /// by `CommitsRefresh` after external branch changes.
    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.loading_previews.clear();
        self.preview_items.clear();
        self.state.update(cx, |s, cx| s.reset_caches(cx));
        let picker = self.picker.clone();
        picker.update(cx, |p, picker_cx| {
            p.delegate_mut().reached_end = false;
            p.delegate_mut()
                .update_matches(String::new(), picker_cx)
                .detach();
        });
        cx.notify();
    }

    /// Kick off a preview load for `sha` when it is neither cached
    /// nor already in flight. Idempotent. The completion handler
    /// runs on the gpui foreground, where it builds the inner
    /// `ReviewSession` from the fetched trees, stashes
    /// `Arc<inner>` in `state.preview_sessions[sha]`, and assembles
    /// the rendered `Entity<ReviewItem>` keyed by sha.
    fn start_preview_load(&mut self, sha: String, cx: &mut Context<'_, Self>) {
        let already_cached = self
            .state
            .read(cx)
            .inner
            .preview_sessions
            .contains_key(&sha);
        if already_cached || self.loading_previews.contains(&sha) {
            return;
        }
        let workdir = self.workdir.clone();
        let git_host = self.git_host.clone();
        let executor = self.executor.clone();
        self.loading_previews.insert(sha.clone());

        let sha_for_task = sha.clone();
        cx.spawn(async move |weak_self, cx| {
            let outcome = executor
                .spawn(async move {
                    let repo = git_host.discover(&workdir)?;
                    Some(fetch_preview_trees(repo.as_ref(), &workdir, &sha_for_task))
                })
                .await;
            let _ = weak_self.update(cx, |this, cx| {
                this.preview_load_landed(sha, outcome, cx);
            });
        })
        .detach();
    }

    fn preview_load_landed(
        &mut self,
        sha: String,
        outcome: Option<PreviewTrees>,
        cx: &mut Context<'_, Self>,
    ) {
        // `loading_previews` stays populated until the very end so the
        // subscription handlers that fire on every `state.update`
        // emit below see the load as still in flight and skip
        // spawning a duplicate.
        let Some(trees) = outcome else {
            self.loading_previews.remove(&sha);
            return;
        };
        let PreviewTrees {
            workdir,
            summary,
            base_tree,
            new_tree,
        } = trees;

        self.state
            .update(cx, |s, cx| s.set_summary(sha.clone(), summary, cx));

        let inputs = build_review_inputs(&workdir, &base_tree, &new_tree, cx);
        if inputs.is_empty() {
            self.loading_previews.remove(&sha);
            return;
        }

        let mut inner_session = InnerReviewSession::new(ReviewSource::Commit {
            workdir,
            sha: sha.clone(),
        });
        inner_session.add_files(inputs);
        if inner_session.order.is_empty() {
            self.loading_previews.remove(&sha);
            return;
        }

        let arc_for_state = Arc::new(inner_session.clone());
        self.state.update(cx, |s, cx| {
            s.set_preview_session(sha.clone(), arc_for_state, cx)
        });

        let session_entity = cx.new(|_| GuiReviewSession::new(inner_session));
        let buffer_registry = self.buffer_registry.clone();
        let review_item =
            cx.new(|cx| ReviewItem::from_session(session_entity, &buffer_registry, cx));
        self.preview_items.insert(sha.clone(), review_item);
        self.loading_previews.remove(&sha);
        cx.notify();
    }
}

/// Trees fetched on the executor for a preview load. The summary is
/// included because `GitRepo::commit_file_changes` is the same
/// (cheap) git call shape and pairing them avoids a second
/// foreground -> executor hop just to fetch the file-stats row.
struct PreviewTrees {
    workdir: PathBuf,
    summary: Vec<CommitFileChange>,
    base_tree: BTreeMap<PathBuf, String>,
    new_tree: BTreeMap<PathBuf, String>,
}

fn fetch_preview_trees(repo: &dyn GitRepo, workdir: &Path, sha: &str) -> PreviewTrees {
    let workdir = repo.workdir().unwrap_or_else(|| workdir.to_path_buf());
    let summary = repo.commit_file_changes(sha);
    let new_tree = repo.commit_tree(sha).unwrap_or_default();
    let base_tree = match repo.parent_sha(sha) {
        Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
        None => BTreeMap::new(),
    };
    PreviewTrees {
        workdir,
        summary,
        base_tree,
        new_tree,
    }
}

fn build_review_inputs(
    workdir: &Path,
    base_tree: &BTreeMap<PathBuf, String>,
    new_tree: &BTreeMap<PathBuf, String>,
    cx: &App,
) -> Vec<ReviewFileInput> {
    let langs = &cx.global::<LanguageRegistry>().0;
    let mut paths: BTreeSet<&Path> = BTreeSet::new();
    for p in base_tree.keys() {
        paths.insert(p.as_path());
    }
    for p in new_tree.keys() {
        paths.insert(p.as_path());
    }
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = langs.for_path(&abs);
        inputs.push(ReviewFileInput {
            path: abs,
            rel_path: rel.display().to_string(),
            language: lang,
            base_text: Arc::new(base),
            buffer_text: Arc::new(buffer),
        });
    }
    inputs
}

/// Picker delegate driven by [`Entity<CommitListState>`]. Reads commit
/// rows out of the shared state for `render_match` and mirrors cursor
/// movement back into the state so the preview pane, future action
/// handlers, and (eventually) workspace persistence all observe the
/// same `selected` index.
///
/// `count`, `selected`, and `reached_end` are cached because the
/// [`PickerDelegate`] trait's `match_count(&self)` /
/// `selected_index(&self)` methods do not receive a context. The
/// owning [`CommitListItem`] keeps them in sync with the inner state
/// via a subscription on [`CommitListStateEvent::Changed`].
///
/// Pagination is driven by [`Self::maybe_spawn_next_page`], reached
/// from both `update_matches` (used by tests and by the
/// `OpenCommits` action handler to kick off the first page) and
/// `set_selected_index` (the scroll trigger). The spawn closure runs
/// `GitRepo::log_commits` on the background executor and posts the
/// page back through `WeakEntity<CommitListState>::update` on
/// completion.
pub struct CommitListDelegate {
    state: Entity<CommitListState>,
    workdir: PathBuf,
    git_host: Arc<dyn GitHost>,
    executor: Executor,
    count: usize,
    selected: usize,
    reached_end: bool,
    pending_load: bool,
}

impl CommitListDelegate {
    pub fn new(state: Entity<CommitListState>, cx: &App) -> Self {
        let (count, selected, reached_end, workdir) = {
            let s = state.read(cx);
            (
                s.inner.commits.len(),
                s.inner.selected,
                s.inner.reached_end,
                s.inner.workdir.clone(),
            )
        };
        let git_host = cx.global::<GitHostGlobal>().0.clone();
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        Self {
            state,
            workdir,
            git_host,
            executor,
            count,
            selected,
            reached_end,
            pending_load: false,
        }
    }

    /// Spawn another commit-log page when the cursor is near the
    /// loaded tail and no load is already in flight. The empty-state
    /// case (`count == 0`) also satisfies the prefetch condition, so
    /// the first call kicks off the initial page.
    fn maybe_spawn_next_page(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        if self.pending_load || self.reached_end {
            return;
        }
        if !(self.count == 0 || self.selected + COMMITS_PREFETCH_GAP >= self.count) {
            return;
        }

        let after = self
            .state
            .read(cx)
            .inner
            .commits
            .last()
            .map(|c| c.sha.clone());
        let workdir = self.workdir.clone();
        let git_host = self.git_host.clone();
        let executor = self.executor.clone();
        let weak_state = self.state.downgrade();

        self.pending_load = true;

        cx.spawn(async move |weak_picker, cx| {
            let page = executor
                .spawn(async move {
                    let Some(repo) = git_host.discover(&workdir) else {
                        return Vec::new();
                    };
                    repo.log_commits(after.as_deref(), COMMITS_PAGE_LIMIT)
                })
                .await;
            let _ = weak_state.update(cx, |s, cx| s.append_page(page, cx));
            let _ = weak_picker.update(cx, |p, _cx| {
                p.delegate_mut().pending_load = false;
            });
        })
        .detach();
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
        self.maybe_spawn_next_page(cx);
    }

    fn update_matches(&mut self, _query: String, cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        // FIXME: query-driven filtering (substring match against
        // sha / summary / author) lands alongside the commit-list
        // action handlers in TODO.md line 21; today the query is
        // ignored and the picker shows every loaded commit.
        self.maybe_spawn_next_page(cx);
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
        let (summary, selected_sha) = {
            let s = self.state.read(cx);
            let sha = s.inner.selected_sha().map(String::from);
            let summary = sha
                .as_deref()
                .and_then(|sha| s.inner.summaries.get(sha))
                .cloned();
            (summary, sha)
        };
        let preview_item = selected_sha
            .as_deref()
            .and_then(|sha| self.preview_items.get(sha))
            .cloned();
        let loading = selected_sha
            .as_deref()
            .map(|sha| self.loading_previews.contains(sha))
            .unwrap_or(false);

        let preview_pane = div()
            .flex()
            .flex_col()
            .size_full()
            .child(render_summary_strip(summary.as_deref()))
            .child(render_preview_body(preview_item, loading));

        div()
            .flex()
            .flex_row()
            .size_full()
            .child(div().flex_1().child(self.picker.clone()))
            .child(div().flex_1().child(preview_pane))
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

fn render_summary_strip(summary: Option<&[CommitFileChange]>) -> impl IntoElement {
    let mut col = div().flex().flex_col();
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
    col
}

fn render_preview_body(
    preview_item: Option<Entity<ReviewItem>>,
    loading: bool,
) -> impl IntoElement {
    let mut wrapper = div().flex_1();
    match (preview_item, loading) {
        (Some(item), _) => {
            wrapper = wrapper.child(item);
        },
        (None, true) => {
            wrapper = wrapper.child("(loading preview...)");
        },
        (None, false) => {
            wrapper = wrapper.child("(no preview session)");
        },
    }
    wrapper
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
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use stoat::host::fake::FakeGit;
    use stoat_scheduler::TestScheduler;

    fn install_globals(cx: &mut TestAppContext, git: Arc<FakeGit>, scheduler: Arc<TestScheduler>) {
        let executor = scheduler.executor();
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
            cx.set_global(LanguageRegistry::standard());
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
        #[allow(dead_code)]
        git: Arc<FakeGit>,
        scheduler: Arc<TestScheduler>,
    }

    impl Harness<'_> {
        /// Drive both the stoat scheduler and gpui's executor to a
        /// fixed-point so pagination tasks (which hop between them)
        /// finish before the test asserts.
        fn settle(&mut self) {
            for _ in 0..4 {
                self.scheduler.run_until_parked();
                self.vcx.run_until_parked();
            }
        }
    }

    fn new_item(cx: &mut TestAppContext, commits: Vec<CommitInfo>) -> Harness<'_> {
        new_item_with_git(cx, commits, Arc::new(FakeGit::new()))
    }

    fn new_item_with_git(
        cx: &mut TestAppContext,
        commits: Vec<CommitInfo>,
        git: Arc<FakeGit>,
    ) -> Harness<'_> {
        let scheduler = Arc::new(TestScheduler::new());
        install_globals(cx, git.clone(), scheduler.clone());
        let state = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerCommitListState::new(PathBuf::from("/repo"));
                inner.commits = commits;
                CommitListState::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let vcx = cx.add_empty_window();
        let item = vcx.update(|window, cx| {
            let state = state.clone();
            let registry = registry.clone();
            cx.new(|cx| CommitListItem::new(state, registry, window, cx))
        });
        Harness {
            item,
            state,
            vcx,
            git,
            scheduler,
        }
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

    fn seed_linear_history(git: &Arc<FakeGit>, workdir: &str, count: usize) {
        let mut builder = git.add_repo(workdir);
        let mut prev: Option<String> = None;
        for i in 0..count {
            let sha = format!("c{:04}", i);
            match prev.as_deref() {
                None => {
                    builder.commit_with_message(
                        &sha,
                        &format!("commit {sha}"),
                        &[("a.rs", "fn a() {}\n")],
                    );
                },
                Some(parent) => {
                    builder.commit_with_parent_message(
                        &sha,
                        parent,
                        &format!("commit {sha}"),
                        &[("a.rs", "fn a() {}\n")],
                    );
                },
            };
            prev = Some(sha);
        }
    }

    #[test]
    fn update_matches_first_call_loads_first_page() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        seed_linear_history(&git, "/repo", 3);
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        h.settle();

        let state_count = h.state.read_with(h.vcx, |s, _| s.inner.commits.len());
        let delegate_count = picker.read_with(h.vcx, |p, _| p.delegate().match_count());
        assert_eq!((state_count, delegate_count), (3, 3));
    }

    #[test]
    fn update_matches_empty_repo_sets_reached_end() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo");
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        h.settle();

        let state_reached = h.state.read_with(h.vcx, |s, _| s.inner.reached_end);
        let delegate_reached = picker.read_with(h.vcx, |p, _| p.delegate().reached_end);
        assert!(state_reached, "state.reached_end should flip on empty page");
        assert!(
            delegate_reached,
            "delegate.reached_end should mirror state.reached_end after settle"
        );
    }

    #[test]
    fn scroll_near_tail_spawns_next_page() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        // Two pages worth of commits so the second batch has somewhere to land.
        seed_linear_history(&git, "/repo", COMMITS_PAGE_LIMIT + 10);
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        h.settle();
        let after_first = h.state.read_with(h.vcx, |s, _| s.inner.commits.len());
        assert_eq!(after_first, COMMITS_PAGE_LIMIT);

        let tail_ix = COMMITS_PAGE_LIMIT - COMMITS_PREFETCH_GAP;
        picker.update(h.vcx, |p, cx| p.set_selected_index(tail_ix, cx));
        h.settle();

        let after_second = h.state.read_with(h.vcx, |s, _| s.inner.commits.len());
        assert_eq!(after_second, COMMITS_PAGE_LIMIT + 10);
    }

    #[test]
    fn update_matches_no_op_while_pending() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        seed_linear_history(&git, "/repo", 3);
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
            // Second call while the first is still pending must not spawn
            // a duplicate load.
            p.delegate_mut().update_matches(String::new(), cx).detach();
            assert!(
                p.delegate().pending_load,
                "first call should have marked pending_load=true",
            );
        });
        h.settle();

        let state_count = h.state.read_with(h.vcx, |s, _| s.inner.commits.len());
        assert_eq!(
            state_count, 3,
            "exactly one page lands even after duplicate update_matches",
        );
    }

    #[test]
    fn reached_end_blocks_further_loads() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo");
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());

        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        h.settle();
        assert!(picker.read_with(h.vcx, |p, _| p.delegate().reached_end));

        let pending_before = picker.read_with(h.vcx, |p, _| p.delegate().pending_load);
        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        let pending_after = picker.read_with(h.vcx, |p, _| p.delegate().pending_load);
        assert_eq!(
            (pending_before, pending_after),
            (false, false),
            "reached_end must prevent another spawn",
        );
    }

    fn seed_repo_with_initial_commit(git: &Arc<FakeGit>, workdir: &str, sha: &str) {
        git.add_repo(workdir).commit_with_message(
            sha,
            &format!("commit {sha}"),
            &[("a.rs", "fn a() {}\n")],
        );
    }

    fn trigger_initial_load(h: &mut Harness<'_>) {
        let picker = h.item.read_with(h.vcx, |item, _| item.picker.clone());
        picker.update(h.vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
        h.settle();
    }

    #[test]
    fn selection_change_spawns_preview_load() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        seed_repo_with_initial_commit(&git, "/repo", "c0000000");
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);

        trigger_initial_load(&mut h);

        let state_has_preview = h.state.read_with(h.vcx, |s, _| {
            s.inner.preview_sessions.contains_key("c0000000")
        });
        let item_has_preview = h.item.read_with(h.vcx, |item, _| {
            item.preview_items().contains_key("c0000000")
        });
        assert!(
            state_has_preview && item_has_preview,
            "state.preview_sessions and preview_items must both carry c0000000",
        );
    }

    #[test]
    fn selection_change_populates_summary() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        seed_repo_with_initial_commit(&git, "/repo", "c0000001");
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);

        trigger_initial_load(&mut h);

        let summary = h
            .state
            .read_with(h.vcx, |s, _| s.inner.summaries.get("c0000001").cloned());
        let changes = summary.expect("summary cached for selected sha");
        assert!(
            !changes.is_empty(),
            "first commit against empty parent tree must report at least one CommitFileChange",
        );
    }

    #[test]
    fn preview_load_dedupes_in_flight() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        seed_repo_with_initial_commit(&git, "/repo", "c0000002");
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);

        trigger_initial_load(&mut h);

        // Second trigger via fresh state event for the same sha must
        // not spawn another task: the cached preview short-circuits.
        h.state.update(h.vcx, |s, cx| s.set_selected(0, cx));
        h.vcx.run_until_parked();
        let loading_after = h
            .item
            .read_with(h.vcx, |item, _| item.loading_previews.clone());
        assert!(
            loading_after.is_empty(),
            "no in-flight load expected after cache hit (got {loading_after:?})",
        );
    }

    #[test]
    fn empty_commit_diff_does_not_cache_preview_item() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        // Two commits with identical trees: diff produces zero hunks.
        git.add_repo("/repo")
            .commit_with_message("c1000000", "first", &[("a.rs", "same\n")])
            .commit_with_parent_message("c1000001", "c1000000", "noop", &[("a.rs", "same\n")]);
        let mut h = new_item_with_git(&mut cx, Vec::new(), git);

        trigger_initial_load(&mut h);

        let item_has_preview = h.item.read_with(h.vcx, |item, _| {
            item.preview_items().contains_key("c1000001")
        });
        let summary_has_entry = h
            .state
            .read_with(h.vcx, |s, _| s.inner.summaries.contains_key("c1000001"));
        assert!(
            !item_has_preview,
            "preview_items must skip the empty-diff commit",
        );
        assert!(
            summary_has_entry,
            "summary still caches even when no hunks are present",
        );
    }
}
