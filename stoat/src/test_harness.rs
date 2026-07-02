#![allow(dead_code)]

pub(crate) mod editor;
pub(crate) mod keys;

use crate::{
    app::{Stoat, UpdateEffect},
    keymap::resolve_config_action,
    keymap_state::arg_as_str,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt::Write,
    sync::Arc,
    time::Duration,
};
use stoat_config::Settings;
use stoat_scheduler::TestScheduler;
use unicode_width::UnicodeWidthStr;

pub struct Frame {
    pub number: usize,
    pub actions: Vec<String>,
    pub mode: String,
    pub size: (u16, u16),
    pub pane_count: usize,
    /// 1-based position of the focused pane in visual traversal order
    /// (left-to-right, top-to-bottom).
    pub focused_pane: usize,
    pub content: String,
}

const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;

/// `(sha, message, files)` triple consumed by [`TestHarness::seed_linear_history`].
/// `files` is itself a slice of `(rel_path, content)` pairs.
pub(crate) type CommitSpec<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

pub struct TestHarness {
    pub(crate) stoat: Stoat,
    #[allow(dead_code)]
    scheduler: Arc<TestScheduler>,
    pub(crate) fake_fs: Arc<crate::host::FakeFs>,
    pub(crate) fake_fs_watcher: Arc<crate::host::FakeFsWatcher>,
    pub(crate) fake_git: Arc<crate::host::FakeGit>,
    pub(crate) fake_env: Arc<crate::host::FakeEnv>,
    pub(crate) fake_lsp: Arc<crate::host::FakeLsp>,
    pub(crate) fake_clipboard: Arc<crate::host::FakeClipboard>,
    pub(crate) fake_terminal: Arc<crate::host::FakeTerminalSession>,
    frames: Vec<Frame>,
    last_buffer: Option<Buffer>,
    step: usize,
    sub_frame: usize,
}

impl TestHarness {
    fn new(width: u16, height: u16) -> Self {
        Self::new_with_settings(width, height, Settings::default())
    }

    pub(crate) fn new_with_settings(width: u16, height: u16, settings: Settings) -> Self {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let fake_fs = Arc::new(crate::host::FakeFs::new());
        let fake_fs_watcher = Arc::new(crate::host::FakeFsWatcher::new());
        fake_fs_watcher.install_on(&fake_fs);
        let fake_git = Arc::new(crate::host::FakeGit::new());
        let fake_env = Arc::new(crate::host::FakeEnv::new());
        let fake_lsp = Arc::new(crate::host::FakeLsp::new());
        fake_lsp.set_executor(executor.clone());
        let fake_clipboard = Arc::new(crate::host::FakeClipboard::new());
        let fake_terminal = Arc::new(crate::host::FakeTerminalSession::new());
        let mut stoat = Stoat::new(executor, settings, std::path::PathBuf::new());
        stoat.persistence_disabled = true;
        stoat.active_workspace_mut().name = String::new();
        stoat.set_fs_host(fake_fs.clone());
        stoat.set_fs_watch_host(fake_fs_watcher.clone());
        stoat.set_git_host(fake_git.clone());
        stoat.set_env_host(fake_env.clone());
        stoat.set_lsp_host(fake_lsp.clone());
        stoat.set_clipboard_host(fake_clipboard.clone());
        stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake_terminal.clone()));
        stoat.update(Event::Resize(width, height));

        let mut harness = Self {
            stoat,
            scheduler,
            fake_fs,
            fake_fs_watcher,
            fake_git,
            fake_env,
            fake_lsp,
            fake_clipboard,
            fake_terminal,
            frames: Vec::new(),
            last_buffer: None,
            step: 0,
            sub_frame: 0,
        };
        harness.capture("resize");
        harness
    }

    /// Expose the [`crate::host::FakeFs`] backing this harness so tests can
    /// seed file content directly. All open-file / read paths through
    /// [`Stoat`] route through this handle.
    pub fn fake_fs(&self) -> &Arc<crate::host::FakeFs> {
        &self.fake_fs
    }

    /// Expose the [`crate::host::FakeFsWatcher`] backing this
    /// harness. Already paired with `fake_fs()` via
    /// [`crate::host::FakeFsWatcher::install_on`], so writes through
    /// the fake fs auto-emit `Modified` events on watched paths;
    /// tests can also call `inject` directly for create/remove/rename
    /// scenarios.
    pub fn fake_fs_watcher(&self) -> &Arc<crate::host::FakeFsWatcher> {
        &self.fake_fs_watcher
    }

    /// Seed a fixture file into the fake filesystem at `path` with the
    /// given `contents`. Convenience over `fake_fs().insert_file(...)` for
    /// the common test pattern of pre-populating one file before driving
    /// the harness; see [`crate::host::FakeFs::insert_file`] for the
    /// underlying semantics including ancestor-directory creation.
    pub fn seed_fixture(&self, path: impl AsRef<std::path::Path>, contents: impl AsRef<[u8]>) {
        self.fake_fs.insert_file(path, contents);
    }

    /// Expose the [`crate::host::FakeGit`] backing this harness. Use its
    /// `add_repo(...).with_fs(&self.fake_fs)` to populate a repository plus
    /// working-tree state for review-mode tests.
    pub fn fake_git(&self) -> &Arc<crate::host::FakeGit> {
        &self.fake_git
    }

    /// Expose the [`crate::host::FakeEnv`] backing this harness so tests
    /// can seed env-var values before driving code that reads them
    /// through `stoat.env_host()`.
    pub fn fake_env(&self) -> &Arc<crate::host::FakeEnv> {
        &self.fake_env
    }

    /// Expose the [`crate::host::FakeLsp`] backing this harness so
    /// tests can seed hovers, completions, definitions, diagnostics,
    /// etc. before driving code that reads them through
    /// `stoat.lsp_host()`.
    pub fn fake_lsp(&self) -> &Arc<crate::host::FakeLsp> {
        &self.fake_lsp
    }

    /// Drives a notification drain so tests that pushed onto
    /// [`crate::host::FakeLsp`] before any `update` event still see
    /// the dispatched state in the next snapshot.
    pub fn drain_lsp(&mut self) {
        self.stoat.drain_lsp_notifications();
    }

    /// Expose the [`crate::host::FakeClipboard`] backing this harness
    /// so tests can read writes that flowed through
    /// `stoat.clipboard_host()`.
    pub fn fake_clipboard(&self) -> &Arc<crate::host::FakeClipboard> {
        &self.fake_clipboard
    }

    /// Expose the [`crate::host::FakeTerminalSession`] the harness installs as
    /// the default terminal host, so tests can read the bytes the run shell
    /// and terminal panes wrote to their PTY.
    pub fn fake_terminal(&self) -> &Arc<crate::host::FakeTerminalSession> {
        &self.fake_terminal
    }

    /// Assert that every host installed on [`Stoat`] still points at the
    /// fake originally constructed by this harness. Detects test code that
    /// swaps a real host (e.g. [`crate::host::LocalFs`]) back in via the
    /// public `set_*_host` setters; comparison is by `Arc` allocation
    /// pointer, so a fresh fake of the same type would still trigger the
    /// panic.
    ///
    /// Does not detect direct `std::fs::*` / `std::env::*` calls in
    /// production code that bypass the `*Host` traits entirely.
    pub fn assert_no_real_io(&self) {
        fn alloc_ptr<T: ?Sized>(arc: &Arc<T>) -> *const () {
            Arc::as_ptr(arc) as *const ()
        }
        assert_eq!(
            alloc_ptr(&self.stoat.fs_host),
            alloc_ptr(&self.fake_fs),
            "FsHost was replaced during the test; real filesystem IO may have escaped"
        );
        assert_eq!(
            alloc_ptr(&self.stoat.fs_watch_host),
            alloc_ptr(&self.fake_fs_watcher),
            "FsWatchHost was replaced during the test; real filesystem watches may have escaped"
        );
        assert_eq!(
            alloc_ptr(&self.stoat.env_host),
            alloc_ptr(&self.fake_env),
            "EnvHost was replaced during the test; real env reads may have escaped"
        );
        assert_eq!(
            alloc_ptr(&self.stoat.git_host),
            alloc_ptr(&self.fake_git),
            "GitHost was replaced during the test; real git operations may have escaped"
        );
        assert_eq!(
            alloc_ptr(&self.stoat.lsp_host),
            alloc_ptr(&self.fake_lsp),
            "LspHost was replaced during the test; real LSP traffic may have escaped"
        );
        assert_eq!(
            alloc_ptr(&self.stoat.clipboard_host),
            alloc_ptr(&self.fake_clipboard),
            "ClipboardHost was replaced during the test; real clipboard writes may have escaped"
        );
    }

    /// Stage a working-tree review scenario in one call.
    ///
    /// Registers `workdir` as the active workspace's `git_root`, populates
    /// the fake git repo with HEAD content for each `(rel_path, head, working)`
    /// tuple, and writes the working version into the fake filesystem so
    /// the review handler reads consistent state. Every entry must have
    /// `head != working`: equal content is not a change and would silently
    /// produce a review with zero hunks.
    pub fn stage_review_scenario(
        &mut self,
        workdir: impl Into<std::path::PathBuf>,
        files: &[(&str, &str, &str)],
    ) {
        let workdir = workdir.into();
        self.stoat.active_workspace_mut().git_root = workdir.clone();
        let mut builder = self.fake_git.add_repo(workdir).with_fs(&self.fake_fs);
        for (rel, head, working) in files {
            builder.modified(rel, head, working);
        }
    }

    /// Dispatch `ReviewApplyStaged` against the current state, then settle so
    /// the post-apply refresh, now an off-loop re-scan, lands before callers
    /// inspect state or snapshot.
    pub fn dispatch_review_apply(&mut self) {
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::ReviewApplyStaged);
        self.settle();
    }

    /// Dispatch `ReviewRefresh` directly (this action is palette-only and not
    /// currently bound to a default key), then settle so the off-loop re-scan
    /// lands before callers inspect state.
    pub fn dispatch_review_refresh(&mut self) {
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::ReviewRefresh);
        self.settle();
    }

    /// Set the status of the chunk at `order_index` in the active review
    /// session. Panics if no session is open or the index is out of range.
    pub(crate) fn set_review_status(
        &mut self,
        order_index: usize,
        status: crate::review_session::ChunkStatus,
    ) {
        let session = self
            .stoat
            .active_workspace_mut()
            .review
            .as_mut()
            .expect("no review session");
        let id = *session
            .order
            .get(order_index)
            .expect("order index out of range");
        session.set_status(id, status);
    }

    /// Variant of [`Self::stage_review_scenario`] that additionally seeds
    /// pre-existing staged files into the fake git repo. Modified files
    /// populate HEAD + unstaged working-tree state; staged entries are
    /// marked staged in the index without a HEAD record.
    pub fn stage_review_scenario_with_staged(
        &mut self,
        workdir: impl Into<std::path::PathBuf>,
        modified: &[(&str, &str, &str)],
        staged: &[(&str, &str)],
    ) {
        let workdir = workdir.into();
        self.stoat.active_workspace_mut().git_root = workdir.clone();
        let mut builder = self.fake_git.add_repo(workdir).with_fs(&self.fake_fs);
        for (rel, head, working) in modified {
            builder.modified(rel, head, working);
        }
        for (rel, working) in staged {
            builder.staged_file(rel, working);
        }
    }

    /// Open a review over a list of agent-proposed edits without touching git.
    /// Each entry is `(rel_path, base_text, proposed_text)`.
    pub fn open_agent_edit_review(&mut self, edits: &[(&str, &str, &str)]) {
        use std::sync::Arc;
        let action = stoat_action::OpenReviewAgentEdits {
            edits: edits
                .iter()
                .map(|(p, base, proposed)| stoat_action::AgentEdit {
                    path: std::path::PathBuf::from(p),
                    base_text: Arc::new((*base).to_string()),
                    proposed_text: Arc::new((*proposed).to_string()),
                })
                .collect(),
        };
        crate::action_handlers::dispatch(&mut self.stoat, &action);
    }

    /// Open a review of a single commit against its first parent.
    pub fn open_commit_review(&mut self, workdir: impl Into<std::path::PathBuf>, sha: &str) {
        let action = stoat_action::OpenReviewCommit {
            workdir: workdir.into(),
            sha: sha.to_string(),
        };
        crate::action_handlers::dispatch(&mut self.stoat, &action);
        self.settle();
    }

    /// Open a review of a commit range (`from`..`to`).
    pub fn open_commit_range_review(
        &mut self,
        workdir: impl Into<std::path::PathBuf>,
        from: &str,
        to: &str,
    ) {
        let action = stoat_action::OpenReviewCommitRange {
            workdir: workdir.into(),
            from: from.to_string(),
            to: to.to_string(),
        };
        crate::action_handlers::dispatch(&mut self.stoat, &action);
        self.settle();
    }

    /// Enter commits mode against `workdir`. Updates the active
    /// workspace's `git_root` so the handler finds the fake repo, then
    /// dispatches `OpenCommits` and settles any async page/preview
    /// loads so the next snapshot sees resolved state.
    pub fn open_commits(&mut self, workdir: impl Into<std::path::PathBuf>) {
        let workdir = workdir.into();
        self.stoat.active_workspace_mut().git_root = workdir;
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::OpenCommits);
        self.settle();
        self.capture("open_commits");
    }

    /// Write `new_text` to the active review session's file matching
    /// `rel`, then drain watcher events into a debounce timer. The
    /// [`crate::host::FakeFsWatcher`] hook installed in
    /// [`Self::new_with_settings`] auto-emits a
    /// [`crate::host::FsEventKind::Modified`] event on every watched
    /// path; [`crate::app::Stoat::drain_fs_watch_events`] consumes
    /// it and arms a 50ms debounce keyed on the path. Callers must
    /// follow up with [`Self::advance_clock`] past
    /// [`crate::app::REVIEW_EXTERNAL_EDIT_DEBOUNCE`] to fire the
    /// timer and land the [`stoat_action::ReviewExternalEdit`]
    /// dispatch; calling `external_edit` repeatedly within the
    /// window coalesces the bursts into one dispatch, matching the
    /// production formatter-on-save behaviour.
    pub(crate) fn external_edit(&mut self, rel: &str, new_text: &str) {
        use crate::host::FsHost;
        let path = {
            let session = self
                .stoat
                .active_workspace()
                .review
                .as_ref()
                .expect("no review session");
            session
                .files
                .iter()
                .find(|f| f.rel_path == rel)
                .unwrap_or_else(|| panic!("review file {rel:?} not found in session"))
                .path
                .clone()
        };
        self.fake_fs
            .write(&path, new_text.as_bytes())
            .expect("FakeFs::write");
        self.stoat.drain_fs_watch_events();
        self.settle();
        self.capture("external_edit");
    }

    /// Inject a [`crate::host::FsEventKind::Created`] event for `rel`
    /// after writing `text`. Models an external tool creating a new
    /// file inside the active review session's `workdir`. Panics for
    /// non-`WorkingTree` sources because their content is not on disk.
    /// The created event arms the same debounce as
    /// [`Self::external_edit`]; callers advance the clock past
    /// [`crate::app::REVIEW_EXTERNAL_EDIT_DEBOUNCE`] to fire the
    /// dispatch.
    pub(crate) fn inject_external_create(&mut self, rel: &str, text: &str) {
        use crate::{
            host::{FsEventKind, FsHost},
            review_session::ReviewSource,
        };
        let workdir = {
            let session = self
                .stoat
                .active_workspace()
                .review
                .as_ref()
                .expect("no review session");
            match &session.source {
                ReviewSource::WorkingTree { workdir } => workdir.clone(),
                other => {
                    panic!("inject_external_create requires a WorkingTree source, got {other:?}")
                },
            }
        };
        let path = workdir.join(rel);
        self.fake_fs
            .write(&path, text.as_bytes())
            .expect("FakeFs::write");
        self.fake_fs_watcher.inject(&path, FsEventKind::Created);
        self.stoat.drain_fs_watch_events();
        self.settle();
        self.capture("inject_external_create");
    }

    /// Read-only access to the active review session via a closure.
    /// Panics when no session is open.
    pub(crate) fn with_review<R>(
        &self,
        f: impl FnOnce(&crate::review_session::ReviewSession) -> R,
    ) -> R {
        let session = self
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .expect("no review session");
        f(session)
    }

    /// Assert the active session's progress matches `expected`. Failure
    /// includes both sides for diff context.
    pub(crate) fn assert_review_progress(&self, expected: crate::review_session::ReviewProgress) {
        let actual = self.with_review(|s| s.progress());
        assert_eq!(actual, expected, "review progress mismatch");
    }

    /// The current review-cursor chunk id. Panics when no session is
    /// open or the cursor has not settled on a chunk yet.
    pub(crate) fn current_review_chunk_id(&self) -> crate::review_session::ReviewChunkId {
        self.with_review(|s| s.cursor.current.expect("no current chunk"))
    }

    /// Status of the chunk identified by `id`. Panics when the chunk
    /// is not in the active session.
    pub(crate) fn chunk_status(
        &self,
        id: crate::review_session::ReviewChunkId,
    ) -> crate::review_session::ChunkStatus {
        self.with_review(|s| s.chunk(id).expect("chunk not found").status)
    }

    pub fn with_size(width: u16, height: u16) -> Self {
        Self::new(width, height)
    }

    pub fn with_settings(settings: Settings) -> Self {
        Self::new_with_settings(DEFAULT_WIDTH, DEFAULT_HEIGHT, settings)
    }

    pub fn open_file(&mut self, path: &std::path::Path) {
        self.stoat.open_file(path);
        self.capture("open_file");
    }

    pub fn type_keys(&mut self, seq: &str) {
        self.step += 100;
        self.sub_frame = 0;

        for key in parse_keys(seq) {
            let desc = key_description(&key);
            if self.stoat.update(Event::Key(key)) == UpdateEffect::Redraw {
                self.maybe_capture(&desc);
            }
        }
    }

    /// Send each char of `text` as an individual `Char(c)` KeyEvent. Bypasses
    /// `parse_keys`, so chars like `-`, `:`, `/` that have special meaning in
    /// the key-token grammar are typed literally.
    pub fn type_text(&mut self, text: &str) {
        self.step += 100;
        self.sub_frame = 0;
        for ch in text.chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            let desc = key_description(&key);
            if let UpdateEffect::Redraw = self.stoat.update(Event::Key(key)) {
                self.maybe_capture(&desc);
            }
        }
    }

    pub fn tick(&mut self) -> bool {
        self.scheduler.tick()
    }

    /// Advance the fake clock by `duration`, firing every timer that
    /// expires inside the window, then settle the harness so any pending
    /// commit produced by those wake-ups is routed through the main dispatch
    /// path before returning.
    pub fn advance_clock(&mut self, duration: Duration) {
        self.scheduler.advance_clock(duration);
        self.settle();
    }

    /// Drive the scheduler and async pump pipeline to a fixed point. After
    /// returning, every spawned task has been polled to suspension and every
    /// queued pump result has been routed through the main dispatch path.
    pub fn settle(&mut self) {
        loop {
            self.scheduler.run_until_parked();
            let commits = crate::action_handlers::pump_commits(&mut self.stoat);
            let review = crate::action_handlers::pump_review_scan(&mut self.stoat);
            let lsp_jumps = crate::action_handlers::pump_lsp_jumps(&mut self.stoat);
            let lsp_hover = crate::action_handlers::lsp::pump_lsp_hover(&mut self.stoat);
            let lsp_code_actions =
                crate::action_handlers::lsp::pump_lsp_code_actions(&mut self.stoat);
            let lsp_code_action_resolve =
                crate::action_handlers::lsp::pump_lsp_code_action_resolve(&mut self.stoat);
            let lsp_prepare_rename =
                crate::action_handlers::lsp::pump_lsp_prepare_rename(&mut self.stoat);
            let lsp_rename = crate::action_handlers::lsp::pump_lsp_rename(&mut self.stoat);
            let lsp_symbol_picker =
                crate::action_handlers::lsp::pump_lsp_symbol_picker(&mut self.stoat);
            let lsp_workspace_symbol =
                crate::action_handlers::lsp::pump_lsp_workspace_symbol(&mut self.stoat);
            let lsp_format = crate::action_handlers::lsp::pump_lsp_format(&mut self.stoat);
            let completion = crate::completion::request::pump(&mut self.stoat);
            let external_edits = self.stoat.drain_pending_external_edits();
            if !commits
                && !review
                && !lsp_jumps
                && !lsp_hover
                && !lsp_code_actions
                && !lsp_code_action_resolve
                && !lsp_prepare_rename
                && !lsp_rename
                && !lsp_symbol_picker
                && !lsp_workspace_symbol
                && !lsp_format
                && !completion
                && !external_edits
            {
                break;
            }
        }
    }

    pub fn snapshot(&mut self) -> &Frame {
        self.capture("snapshot");
        self.frames.last().expect("no frames captured")
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.step += 100;
        self.sub_frame = 0;
        self.stoat.update(Event::Resize(width, height));
        self.capture("resize");
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Snapshot the rendered pane: characters plus inline ANSI SGR escapes
    /// for foreground/background color, modifiers (bold, reverse, etc.),
    /// and the cursor cell. Asserts positioning, style, and cursor state
    /// together.
    pub fn assert_snapshot(&mut self, name: &str) {
        self.capture("snapshot");
        let frame = self.frames.last().expect("no frames");
        let buf = self.last_buffer.as_ref().expect("no buffer");
        let text = format_styled(frame, buf);
        insta::assert_snapshot!(name, text);
    }

    /// Snapshot one production-style frame from a single `drive_background`
    /// and one `render`, with no settle loop or second frame.
    ///
    /// The real event loop runs one `drive_background` plus one paint per
    /// redraw, so this reflects what the user sees on the first idle frame
    /// after a state change. [`Self::assert_snapshot`] instead settles the
    /// scheduler and renders twice, which hides work that only completes once
    /// the parse scheduler has been pumped past the first frame.
    pub fn assert_snapshot_one_frame(&mut self, name: &str) {
        self.stoat.drive_background();
        let buf = self.stoat.render();
        let (pane_count, focused_pane) = self.pane_metadata();
        let frame = Frame {
            number: self.step + self.sub_frame,
            actions: vec!["one_frame".to_string()],
            mode: self.stoat.mode.clone(),
            size: (buf.area.width, buf.area.height),
            pane_count,
            focused_pane,
            content: buffer_to_text(&buf),
        };
        let text = format_styled(&frame, &buf);
        insta::assert_snapshot!(name, text);
    }

    /// Edit the focused buffer at the given byte range, replacing it with
    /// `text`. Triggers a capture so the next render reflects the edit.
    /// Test-only helper for exercising the incremental reparse path.
    pub fn edit_focused(&mut self, range: std::ops::Range<usize>, text: &str) {
        let ws = self.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("edit_focused: focused pane is not an editor"),
        };
        let editor = ws.editors.get(editor_id).expect("focused editor exists");
        let buffer = ws.buffers.get(editor.buffer_id).expect("buffer exists");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(range, text);
        }
        self.capture("edit");
    }

    /// Fold the given buffer-point range on the focused editor's display
    /// map, triggering a capture afterward. Test-only helper for exercising
    /// the fold path of the chunks pipeline.
    pub fn fold_focused(&mut self, range: std::ops::Range<stoat_text::Point>) {
        let ws = self.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("fold_focused: focused pane is not an editor"),
        };
        let editor = ws
            .editors
            .get_mut(editor_id)
            .expect("focused editor exists");
        editor.display_map.fold(vec![range]);
        self.capture("fold");
    }

    /// Set up a side-by-side review view from raw text pairs.
    ///
    /// Each entry is `(file_path, base_content, new_content)`. The
    /// structural diff is computed per file, hunks are extracted, and
    /// the review is displayed in the focused pane. No git repo needed.
    pub fn open_review_from_texts(&mut self, files: &[(&str, &str, &str)]) {
        use crate::{
            action_handlers,
            review::ReviewFileInput,
            review_session::{InMemoryFile, ReviewSession, ReviewSource},
        };
        use std::path::PathBuf;

        let in_memory = files
            .iter()
            .map(|(p, b, n)| InMemoryFile {
                path: PathBuf::from(p),
                base_text: Arc::new(b.to_string()),
                buffer_text: Arc::new(n.to_string()),
            })
            .collect::<Vec<_>>();

        let mut session = ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(in_memory.clone()),
        });
        let inputs: Vec<ReviewFileInput> = in_memory
            .iter()
            .map(|file| ReviewFileInput {
                path: file.path.clone(),
                rel_path: file.path.display().to_string(),
                language: self.stoat.language_registry.for_path(&file.path),
                base_text: file.base_text.clone(),
                buffer_text: file.buffer_text.clone(),
            })
            .collect();
        session.add_files(inputs);

        if session.order.is_empty() {
            return;
        }
        action_handlers::install_review_session(&mut self.stoat, session);
        self.capture("open_review");
    }

    pub fn open_run(&mut self) -> crate::run::RunId {
        use crate::pane::View;
        self.type_action("OpenRun()");
        let ws = self.stoat.active_workspace();
        let focused = ws.panes.focus();
        match ws.panes.pane(focused).view {
            View::Run(id) => id,
            _ => panic!("open_run: focused pane is not a Run"),
        }
    }

    pub fn submit_run(&mut self, text: &str) {
        use crate::{pane::View, run::OutputBlock};
        let ws = self.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let View::Run(id) = ws.panes.pane(focused).view else {
            panic!("submit_run: focused pane is not a Run");
        };
        let run_state = ws.runs.get_mut(id).expect("run state exists");
        run_state.history.push(text.to_owned());
        let width = ws.panes.pane(focused).area.width.saturating_sub(2).max(20);
        run_state
            .blocks
            .push(OutputBlock::new(text.to_owned(), width));
        self.capture("submit_run");
    }

    pub fn inject_run_output(&mut self, run_id: crate::run::RunId, data: &[u8]) {
        let notif = crate::run::PtyNotification::Output {
            run_id,
            data: data.to_vec(),
        };
        self.stoat.handle_pty_notification(notif);
        self.capture("inject_output");
    }

    pub fn inject_run_done(&mut self, run_id: crate::run::RunId, exit_code: i32) {
        let mark = format!("\x1b]133;D;{exit_code}\x07");
        let notif = crate::run::PtyNotification::Output {
            run_id,
            data: mark.into_bytes(),
        };
        self.stoat.handle_pty_notification(notif);
        self.capture("inject_done");
    }

    pub fn type_action(&mut self, action_expr: &str) {
        let parsed = stoat_config::parse_action(action_expr)
            .unwrap_or_else(|e| panic!("failed to parse action {action_expr:?}: {e:?}"));
        let target = resolve_config_action(&parsed);
        let tokens = self.find_action_keys(&target, action_expr);
        self.type_keys(&tokens.join(" "));
    }

    fn find_action_keys(
        &self,
        target: &crate::keymap::ResolvedAction,
        action_expr: &str,
    ) -> Vec<String> {
        let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
        let mut visited: HashSet<String> = HashSet::new();

        let start = self.stoat.mode.clone();
        queue.push_back((start.clone(), Vec::new()));
        visited.insert(start);

        while let Some((mode, path)) = queue.pop_front() {
            let bindings = self.stoat.active_keys_for_mode(&mode);

            for (key, actions) in &bindings {
                if actions.iter().any(|a| a == target) {
                    let mut full_path = path.clone();
                    full_path.push(key.to_key_token());
                    return full_path;
                }

                for action in *actions {
                    if action.name == "SetMode"
                        && let Some(target_mode) = action.args.first().and_then(arg_as_str)
                        && visited.insert(target_mode.clone())
                    {
                        let mut new_path = path.clone();
                        new_path.push(key.to_key_token());
                        queue.push_back((target_mode, new_path));
                    }
                }
            }
        }

        panic!(
            "action {action_expr:?} is unreachable from mode {:?}",
            self.stoat.mode
        );
    }

    fn pane_metadata(&self) -> (usize, usize) {
        let ws = self.stoat.active_workspace();
        let focused_id = ws.panes.focus();
        let pane_count = ws.panes.pane_count();
        let focused_pos = ws
            .panes
            .split_panes()
            .position(|(id, _)| id == focused_id)
            .map(|i| i + 1)
            .unwrap_or(0);
        (pane_count, focused_pos)
    }

    fn maybe_capture(&mut self, action: &str) {
        self.stoat.drive_background();
        let _ = self.stoat.render();
        self.scheduler.run_until_parked();
        self.stoat.drive_background();
        let buf = self.stoat.render();
        if self.last_buffer.as_ref() == Some(&buf) {
            if let Some(last) = self.frames.last_mut() {
                last.actions.push(action.to_string());
            }
            return;
        }
        let (pane_count, focused_pane) = self.pane_metadata();
        self.last_buffer = Some(buf.clone());
        self.frames.push(Frame {
            number: self.step + self.sub_frame,
            actions: vec![action.to_string()],
            mode: self.stoat.mode.clone(),
            size: (buf.area.width, buf.area.height),
            pane_count,
            focused_pane,
            content: buffer_to_text(&buf),
        });
        self.sub_frame += 1;
    }

    /// Insert a fresh workspace into the app's slot map and return its id.
    /// The new workspace is not automatically made active; call
    /// [`Self::set_active_workspace`] to switch.
    pub(crate) fn create_workspace(&mut self) -> crate::workspace::WorkspaceId {
        let mut ws =
            crate::workspace::Workspace::new(std::path::PathBuf::new(), &self.stoat.executor);
        ws.name = String::new();
        let id = self.stoat.workspaces.insert(ws);
        self.stoat.workspaces[id].id = id;
        id
    }

    pub(crate) fn set_active_workspace(&mut self, id: crate::workspace::WorkspaceId) {
        self.stoat.active_workspace = id;
    }

    /// Seed a file in the harness' fake filesystem under `/test/<name>`
    /// and return its absolute path. All open-file / read paths through
    /// [`Stoat`] route through [`crate::host::FakeFs`].
    pub(crate) fn write_file(&self, name: &str, content: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from("/test").join(name);
        self.fake_fs.insert_file(&path, content.as_bytes());
        path
    }

    /// Seed an N-line text file (`line 001`, `line 002`, ...) into the
    /// fake filesystem at `/test/<name>`. Convenient for scenarios that
    /// need a file long enough to scroll.
    pub(crate) fn seed_long_file(&self, name: &str, lines: usize) -> std::path::PathBuf {
        let content: String = (1..=lines)
            .map(|i| format!("line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.write_file(name, &content)
    }

    /// Seed a linear chain of commits into the fake git repo at `workdir`.
    /// Each entry is `(sha, message, files)` where `files` is a list of
    /// `(rel_path, content)`. Each commit's parent is the previous entry.
    pub(crate) fn seed_linear_history(&self, workdir: &str, commits: &[CommitSpec<'_>]) {
        let mut builder = self.fake_git.add_repo(workdir);
        let mut prev: Option<&str> = None;
        for (sha, message, files) in commits {
            match prev {
                None => builder.commit_with_message(sha, message, files),
                Some(parent) => builder.commit_with_parent_message(sha, parent, message, files),
            };
            prev = Some(sha);
        }
    }

    /// Append `text` at offset 0 in the focused editor's buffer. Panics
    /// if the focused pane is not an editor.
    pub(crate) fn seed_focused_buffer(&mut self, text: &str) {
        editor::seed_focused_buffer(&mut self.stoat, text);
    }

    /// Resolved byte offsets for each selection's head in the focused
    /// editor.
    pub(crate) fn head_offsets(&mut self) -> Vec<usize> {
        editor::head_offsets(&mut self.stoat)
    }

    /// Resolved `(start, end, reversed)` byte offsets for each selection
    /// in the focused editor.
    pub(crate) fn selection_spans(&mut self) -> Vec<(usize, usize, bool)> {
        editor::selection_spans(&mut self.stoat)
    }

    /// Byte offset of the primary selection's head in the focused editor.
    pub(crate) fn primary_head_offset(&mut self) -> usize {
        editor::primary_head_offset(&mut self.stoat)
    }

    /// Display-grid `(row, column)` for each selection's head in the
    /// focused editor.
    pub(crate) fn cursor_display_positions(&mut self) -> Vec<(u32, u32)> {
        editor::cursor_display_positions(&mut self.stoat)
    }

    /// `scroll_row` for every editor in the active workspace.
    pub(crate) fn editor_scroll_rows(&self) -> Vec<u32> {
        editor::editor_scroll_rows(&self.stoat)
    }

    /// First split-pane that holds an editor view. Panics if none.
    pub(crate) fn editor_pane(&self) -> crate::pane::PaneId {
        editor::editor_pane(&self.stoat)
    }

    /// `EditorId` held by `pane`. Panics if the pane is not an editor.
    pub(crate) fn editor_id_in_pane(
        &self,
        pane: crate::pane::PaneId,
    ) -> crate::editor_state::EditorId {
        editor::editor_id_in_pane(&self.stoat, pane)
    }

    /// `scroll_row` for a specific editor in the active workspace.
    pub(crate) fn editor_scroll_row(&self, editor_id: crate::editor_state::EditorId) -> u32 {
        editor::editor_scroll_row(&self.stoat, editor_id)
    }

    pub(crate) fn capture(&mut self, action: &str) {
        // First render spawns any pending parse jobs. Settling the test
        // scheduler runs them to completion. The second render polls the
        // results and installs them so the snapshot reflects them.
        self.stoat.drive_background();
        let _ = self.stoat.render();
        self.scheduler.run_until_parked();
        self.stoat.drive_background();
        let buf = self.stoat.render();
        let is_different = self.last_buffer.as_ref() != Some(&buf);
        self.last_buffer = Some(buf.clone());
        if is_different {
            let (pane_count, focused_pane) = self.pane_metadata();
            self.frames.push(Frame {
                number: self.step + self.sub_frame,
                actions: vec![action.to_string()],
                mode: self.stoat.mode.clone(),
                size: (buf.area.width, buf.area.height),
                pane_count,
                focused_pane,
                content: buffer_to_text(&buf),
            });
            self.sub_frame += 1;
        }
    }
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }
}

fn buffer_to_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut lines = Vec::with_capacity(area.height as usize);
    for y in area.y..area.y + area.height {
        let mut line = String::with_capacity(area.width as usize);
        for x in area.x..area.x + area.width {
            let symbol = buf[(x, y)].symbol();
            if symbol.is_empty() {
                continue;
            }
            line.push_str(symbol);
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn format_header(frame: &Frame) -> String {
    let mut pairs = BTreeMap::new();
    pairs.insert("actions", frame.actions.join(", "));
    pairs.insert(
        "focused",
        format!("#{} of {}", frame.focused_pane, frame.pane_count),
    );
    pairs.insert("mode", frame.mode.clone());
    pairs.insert("size", format!("{}x{}", frame.size.0, frame.size.1));
    pairs
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_styled(frame: &Frame, buf: &Buffer) -> String {
    let header = format_header(frame);
    let ansi = buffer_to_ansi(buf);
    format!("{header}\n---\n{ansi}")
}

fn buffer_to_ansi(buf: &Buffer) -> String {
    let area = buf.area;
    let default_style = (Color::Reset, Color::Reset, Modifier::empty());
    let mut current = default_style;
    let mut lines: Vec<String> = Vec::with_capacity(area.height as usize);

    for y in area.y..area.y + area.height {
        let rightmost = find_rightmost_visible(buf, y);
        let mut line = String::new();
        let mut skip = 0u16;

        for x in area.x..rightmost {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buf[(x, y)];
            let cell_style = (cell.fg, cell.bg, cell.modifier);

            if cell_style != current {
                line.push_str("\x1b[0m");
                push_sgr(&mut line, cell.fg, cell.bg, cell.modifier);
                current = cell_style;
            }

            let symbol = cell.symbol();
            line.push_str(symbol);
            let w = UnicodeWidthStr::width(symbol);
            if w > 1 {
                skip = (w as u16) - 1;
            }
        }

        if current != default_style {
            line.push_str("\x1b[0m");
            current = default_style;
        }

        lines.push(line);
    }

    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn find_rightmost_visible(buf: &Buffer, y: u16) -> u16 {
    let area = buf.area;
    let mut rightmost = area.x;
    for x in area.x..area.x + area.width {
        let cell = &buf[(x, y)];
        let is_default_space = cell.symbol() == " "
            && cell.fg == Color::Reset
            && cell.bg == Color::Reset
            && cell.modifier.is_empty();
        if !is_default_space {
            rightmost = x + 1;
        }
    }
    rightmost
}

fn push_sgr(out: &mut String, fg: Color, bg: Color, modifier: Modifier) {
    if modifier.contains(Modifier::BOLD) {
        out.push_str("\x1b[1m");
    }
    if modifier.contains(Modifier::DIM) {
        out.push_str("\x1b[2m");
    }
    if modifier.contains(Modifier::ITALIC) {
        out.push_str("\x1b[3m");
    }
    if modifier.contains(Modifier::UNDERLINED) {
        out.push_str("\x1b[4m");
    }
    if modifier.contains(Modifier::SLOW_BLINK) {
        out.push_str("\x1b[5m");
    }
    if modifier.contains(Modifier::RAPID_BLINK) {
        out.push_str("\x1b[6m");
    }
    if modifier.contains(Modifier::REVERSED) {
        out.push_str("\x1b[7m");
    }
    if modifier.contains(Modifier::HIDDEN) {
        out.push_str("\x1b[8m");
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        out.push_str("\x1b[9m");
    }
    push_color_fg(out, fg);
    push_color_bg(out, bg);
}

fn push_color_fg(out: &mut String, color: Color) {
    match color {
        Color::Reset => {},
        Color::Black => out.push_str("\x1b[30m"),
        Color::Red => out.push_str("\x1b[31m"),
        Color::Green => out.push_str("\x1b[32m"),
        Color::Yellow => out.push_str("\x1b[33m"),
        Color::Blue => out.push_str("\x1b[34m"),
        Color::Magenta => out.push_str("\x1b[35m"),
        Color::Cyan => out.push_str("\x1b[36m"),
        Color::Gray => out.push_str("\x1b[37m"),
        Color::DarkGray => out.push_str("\x1b[90m"),
        Color::LightRed => out.push_str("\x1b[91m"),
        Color::LightGreen => out.push_str("\x1b[92m"),
        Color::LightYellow => out.push_str("\x1b[93m"),
        Color::LightBlue => out.push_str("\x1b[94m"),
        Color::LightMagenta => out.push_str("\x1b[95m"),
        Color::LightCyan => out.push_str("\x1b[96m"),
        Color::White => out.push_str("\x1b[97m"),
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[38;5;{n}m");
        },
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
        },
    }
}

fn push_color_bg(out: &mut String, color: Color) {
    match color {
        Color::Reset => {},
        Color::Black => out.push_str("\x1b[40m"),
        Color::Red => out.push_str("\x1b[41m"),
        Color::Green => out.push_str("\x1b[42m"),
        Color::Yellow => out.push_str("\x1b[43m"),
        Color::Blue => out.push_str("\x1b[44m"),
        Color::Magenta => out.push_str("\x1b[45m"),
        Color::Cyan => out.push_str("\x1b[46m"),
        Color::Gray => out.push_str("\x1b[47m"),
        Color::DarkGray => out.push_str("\x1b[100m"),
        Color::LightRed => out.push_str("\x1b[101m"),
        Color::LightGreen => out.push_str("\x1b[102m"),
        Color::LightYellow => out.push_str("\x1b[103m"),
        Color::LightBlue => out.push_str("\x1b[104m"),
        Color::LightMagenta => out.push_str("\x1b[105m"),
        Color::LightCyan => out.push_str("\x1b[106m"),
        Color::White => out.push_str("\x1b[107m"),
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[48;5;{n}m");
        },
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[48;2;{r};{g};{b}m");
        },
    }
}

fn parse_keys(seq: &str) -> Vec<KeyEvent> {
    seq.split_whitespace().map(parse_single_key).collect()
}

fn parse_single_key(token: &str) -> KeyEvent {
    let parts: Vec<&str> = token.split('-').collect();
    if parts.len() == 1 {
        let (code, mods) = resolve_token(parts[0]);
        return KeyEvent::new(code, mods);
    }

    let mut modifiers = KeyModifiers::empty();
    for &part in &parts[..parts.len() - 1] {
        match part.to_lowercase().as_str() {
            "ctrl" | "c" => modifiers |= KeyModifiers::CONTROL,
            "shift" | "s" => modifiers |= KeyModifiers::SHIFT,
            "alt" | "a" => modifiers |= KeyModifiers::ALT,
            _ => {},
        }
    }
    let (code, extra_mods) = resolve_token(parts[parts.len() - 1]);
    KeyEvent::new(code, modifiers | extra_mods)
}

fn resolve_token(token: &str) -> (KeyCode, KeyModifiers) {
    match token.to_lowercase().as_str() {
        "space" => (KeyCode::Char(' '), KeyModifiers::NONE),
        "escape" | "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "enter" | "return" => (KeyCode::Enter, KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "backtab" => (KeyCode::BackTab, KeyModifiers::SHIFT),
        "backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
        "delete" | "del" => (KeyCode::Delete, KeyModifiers::NONE),
        "up" => (KeyCode::Up, KeyModifiers::NONE),
        "down" => (KeyCode::Down, KeyModifiers::NONE),
        "left" => (KeyCode::Left, KeyModifiers::NONE),
        "right" => (KeyCode::Right, KeyModifiers::NONE),
        "home" => (KeyCode::Home, KeyModifiers::NONE),
        "end" => (KeyCode::End, KeyModifiers::NONE),
        "pageup" => (KeyCode::PageUp, KeyModifiers::NONE),
        "pagedown" => (KeyCode::PageDown, KeyModifiers::NONE),
        s if s.starts_with('f') && s.len() > 1 => {
            if let Ok(n) = s[1..].parse::<u8>() {
                return (KeyCode::F(n), KeyModifiers::NONE);
            }
            (
                KeyCode::Char(token.chars().next().expect("empty token")),
                KeyModifiers::NONE,
            )
        },
        _ => {
            let ch = token.chars().next().expect("empty token");
            (KeyCode::Char(ch), KeyModifiers::NONE)
        },
    }
}

fn key_description(event: &KeyEvent) -> String {
    let mut parts = Vec::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    let key_name = match event.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => "?".to_string(),
    };
    parts.push(key_name);
    parts.join("-")
}

/// Two hunks separated by unchanged context; cursor defaults to the
/// first chunk. Verifies the cyan current-chunk gutter and the
/// default progress footer.
pub(crate) const REVIEW_TWO_HUNK_BASE: &str =
    "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
pub(crate) const REVIEW_TWO_HUNK_BUFFER: &str =
    "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nT\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys_single_char() {
        let keys = parse_keys("s");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].code, KeyCode::Char('s'));
        assert_eq!(keys[0].modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_keys_space() {
        let keys = parse_keys("space");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].code, KeyCode::Char(' '));
    }

    #[test]
    fn parse_keys_sequence() {
        let keys = parse_keys("space s r");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].code, KeyCode::Char(' '));
        assert_eq!(keys[1].code, KeyCode::Char('s'));
        assert_eq!(keys[2].code, KeyCode::Char('r'));
    }

    #[test]
    fn parse_keys_ctrl() {
        let keys = parse_keys("ctrl-c");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].code, KeyCode::Char('c'));
        assert!(keys[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_keys_shorthand() {
        let keys = parse_keys("C-k");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].code, KeyCode::Char('k'));
        assert!(keys[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_keys_named() {
        let esc = parse_keys("escape");
        assert_eq!(esc[0].code, KeyCode::Esc);

        let enter = parse_keys("enter");
        assert_eq!(enter[0].code, KeyCode::Enter);
    }

    #[test]
    fn buffer_to_text_basic() {
        let buf = Buffer::with_lines(["hello", "world"]);
        let text = buffer_to_text(&buf);
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn buffer_to_text_trims() {
        let buf = Buffer::with_lines(["hi   ", "     "]);
        let text = buffer_to_text(&buf);
        assert_eq!(text, "hi");
    }

    #[test]
    fn harness_initial_frame() {
        let h = Stoat::test();
        assert_eq!(h.frames().len(), 1);
        assert_eq!(h.frames()[0].mode, "normal");
        assert_eq!(h.frames()[0].size, (80, 24));
        assert_eq!(h.frames()[0].number, 0);
    }

    #[test]
    fn harness_type_keys_mode_change() {
        let mut h = Stoat::test();
        h.type_keys("space");
        assert!(h.frames().len() >= 2);
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.mode, "space");
    }

    #[test]
    fn harness_escape_returns() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.type_keys("escape");
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.mode, "normal");
    }

    #[test]
    fn harness_frame_numbering() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.type_keys("escape");
        let nums: Vec<_> = h.frames().iter().map(|f| f.number).collect();
        assert_eq!(nums[0], 0);
        assert!(nums[1] >= 100 && nums[1] < 200);
        assert!(nums[2] >= 200 && nums[2] < 300);
    }

    #[test]
    fn harness_no_duplicate_frame() {
        let mut h = Stoat::test();
        let before = h.frames().len();
        h.type_keys("f12");
        assert_eq!(h.frames().len(), before);
    }

    #[test]
    fn harness_resize() {
        let mut h = Stoat::test();
        h.resize(100, 30);
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.size, (100, 30));
    }

    #[test]
    fn harness_custom_size() {
        let h = TestHarness::with_size(120, 40);
        assert_eq!(h.frames()[0].size, (120, 40));
    }

    #[test]
    fn to_key_token_round_trips() {
        use crate::keymap::CompiledKey;

        let cases = [
            (KeyCode::Char('q'), KeyModifiers::NONE, "q"),
            (KeyCode::Char(' '), KeyModifiers::NONE, "space"),
            (KeyCode::Esc, KeyModifiers::NONE, "escape"),
            (KeyCode::Enter, KeyModifiers::NONE, "enter"),
            (KeyCode::Char('s'), KeyModifiers::CONTROL, "ctrl-s"),
            (KeyCode::F(1), KeyModifiers::NONE, "f1"),
        ];
        for (code, modifiers, expected) in cases {
            let ck = CompiledKey { code, modifiers };
            let token = ck.to_key_token();
            assert_eq!(token, expected);
            let parsed = parse_single_key(&token);
            assert_eq!(parsed.code, code);
            assert_eq!(parsed.modifiers, modifiers);
        }
    }

    #[test]
    fn assert_no_real_io_passes_for_default_harness() {
        let h = TestHarness::with_size(80, 24);
        h.assert_no_real_io();
    }

    #[test]
    #[should_panic(expected = "FsHost was replaced")]
    fn assert_no_real_io_panics_when_fs_host_swapped() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_fs_host(Arc::new(crate::host::LocalFs));
        h.assert_no_real_io();
    }

    #[test]
    #[should_panic(expected = "EnvHost was replaced")]
    fn assert_no_real_io_panics_when_env_host_swapped() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_env_host(Arc::new(crate::host::LocalEnv));
        h.assert_no_real_io();
    }

    #[test]
    #[should_panic(expected = "GitHost was replaced")]
    fn assert_no_real_io_panics_when_git_host_swapped() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_git_host(Arc::new(crate::host::LocalGit::new()));
        h.assert_no_real_io();
    }

    #[test]
    #[should_panic(expected = "LspHost was replaced")]
    fn assert_no_real_io_panics_when_lsp_host_swapped() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_lsp_host(Arc::new(crate::host::NoopLsp));
        h.assert_no_real_io();
    }
}
