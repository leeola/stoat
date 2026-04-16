#![allow(dead_code)]

mod claude;

use crate::{
    app::{arg_as_str, Stoat, UpdateEffect},
    keymap::resolve_config_action,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt::Write,
    sync::Arc,
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

impl Frame {
    pub fn display(&self) -> String {
        format_plain(self)
    }
}

const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;

pub struct TestHarness {
    pub(crate) stoat: Stoat,
    #[allow(dead_code)]
    scheduler: Arc<TestScheduler>,
    pub(crate) fake_claude_host: Arc<crate::host::FakeClaudeCodeHost>,
    pub(crate) fake_fs: Arc<crate::host::FakeFs>,
    pub(crate) fake_git: Arc<crate::host::FakeGit>,
    pub(crate) claude_fakes:
        HashMap<crate::host::ClaudeSessionId, Arc<crate::host::FakeClaudeCode>>,
    pub(crate) claude_tool_id_counter: u64,
    frames: Vec<Frame>,
    last_buffer: Option<Buffer>,
    step: usize,
    sub_frame: usize,
}

impl TestHarness {
    fn new(width: u16, height: u16) -> Self {
        Self::new_with_settings(width, height, Settings::default())
    }

    fn new_with_settings(width: u16, height: u16, settings: Settings) -> Self {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let fake_claude_host = Arc::new(crate::host::FakeClaudeCodeHost::new());
        let fake_fs = Arc::new(crate::host::FakeFs::new());
        let fake_git = Arc::new(crate::host::FakeGit::new());
        let mut stoat = Stoat::new(executor, settings, std::path::PathBuf::new());
        stoat.set_claude_code_host(fake_claude_host.clone());
        stoat.set_fs_host(fake_fs.clone());
        stoat.set_git_host(fake_git.clone());
        stoat.update(Event::Resize(width, height));

        let mut harness = Self {
            stoat,
            scheduler,
            fake_claude_host,
            fake_fs,
            fake_git,
            claude_fakes: HashMap::new(),
            claude_tool_id_counter: 0,
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

    /// Expose the [`crate::host::FakeGit`] backing this harness. Use its
    /// `add_repo(...).with_fs(&self.fake_fs)` to populate a repository plus
    /// working-tree state for review-mode tests.
    pub fn fake_git(&self) -> &Arc<crate::host::FakeGit> {
        &self.fake_git
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

    /// Dispatch `ReviewApplyStaged` against the current state. Does not
    /// drive rendering; pair with [`Self::settle`] when a caller needs
    /// the snapshot to reflect post-apply state.
    pub fn dispatch_review_apply(&mut self) {
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::ReviewApplyStaged);
    }

    /// Dispatch `ReviewRefresh` directly (this action is palette-only and
    /// not currently bound to a default key).
    pub fn dispatch_review_refresh(&mut self) {
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::ReviewRefresh);
    }

    /// Set the status of the chunk at `order_index` in the active review
    /// session. Panics if no session is open or the index is out of range.
    pub fn set_review_status(
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

    /// Drive the scheduler and Claude notification pipeline to a fixed
    /// point. After returning, every spawned task has been polled to
    /// suspension and every queued [`crate::host::ClaudeNotification`] has
    /// been routed through the main dispatch path.
    pub fn settle(&mut self) {
        loop {
            self.scheduler.run_until_parked();
            if !self.stoat.drain_claude_notifications() {
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

    /// Snapshot the **layout** view: characters and structure with all
    /// styling stripped. Asserts positioning, wrapping, and text content.
    /// Use as the default for most tests; diffs stay readable and the
    /// snapshot is stable across color/theme tweaks.
    pub fn assert_snapshot(&mut self, name: &str) {
        self.capture("snapshot");
        let text = format_plain(self.frames.last().expect("no frames"));
        insta::with_settings!({snapshot_path => "snapshots/tui"}, {
            insta::assert_snapshot!(name, text);
        });
    }

    /// Snapshot the **styled** view: characters plus inline ANSI SGR escapes
    /// for foreground/background color, modifiers (bold, reverse, etc.), and
    /// the cursor cell. Use when colors, highlights, selection bars, or
    /// cursor position carry meaning the layout view cannot represent. Pair
    /// with `assert_snapshot` rather than replacing it.
    pub fn assert_snapshot_styled(&mut self, name: &str) {
        self.capture("snapshot");
        let frame = self.frames.last().expect("no frames");
        let buf = self.last_buffer.as_ref().expect("no buffer");
        let text = format_styled(frame, buf);
        insta::with_settings!({snapshot_path => "snapshots/tui"}, {
            insta::assert_snapshot!(name, text);
        });
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
        for file in &in_memory {
            let lang = self.stoat.language_registry.for_path(&file.path);
            let rel_path = file.path.display().to_string();
            session.add_file(
                file.path.clone(),
                rel_path,
                lang,
                file.base_text.clone(),
                file.buffer_text.clone(),
            );
        }

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
        let notif = crate::run::PtyNotification::CommandDone {
            run_id,
            exit_status: Some(exit_code),
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
                    if action.name == "SetMode" {
                        if let Some(target_mode) = action.args.first().and_then(arg_as_str) {
                            if visited.insert(target_mode.clone()) {
                                let mut new_path = path.clone();
                                new_path.push(key.to_key_token());
                                queue.push_back((target_mode, new_path));
                            }
                        }
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
        let _ = self.stoat.render();
        self.scheduler.run_until_parked();
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

    pub fn open_claude_with_fake(
        &mut self,
        fake: crate::host::FakeClaudeCode,
    ) -> crate::host::ClaudeSessionId {
        let arc = self.fake_claude_host.push_session(fake);
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::OpenClaude);
        self.settle();
        let id = self
            .stoat
            .active_workspace()
            .claude_chat
            .expect("OpenClaude should set claude_chat");
        self.claude_fakes.insert(id, arc);
        self.capture("open_claude");
        id
    }

    pub fn create_background_session(
        &mut self,
        fake: crate::host::FakeClaudeCode,
    ) -> crate::host::ClaudeSessionId {
        let arc = Arc::new(fake);
        let id = self.stoat.claude_sessions_mut().reserve_slot();
        let session: Arc<dyn crate::host::ClaudeCodeSession> = arc.clone();
        self.stoat
            .claude_sessions_mut()
            .fill_slot(id, session.clone());
        // Spawn the polling task for this session so pushes flow through
        // the real notification pipeline. Mirrors what the
        // `SessionReady` notification does for OpenClaude-driven sessions.
        let claude_tx = self.stoat.claude_tx.clone();
        self.stoat
            .executor
            .spawn(crate::app::claude_polling_task(id, session, claude_tx))
            .detach();
        self.claude_fakes.insert(id, arc);
        id
    }

    pub fn show_claude_session(&mut self, session_id: crate::host::ClaudeSessionId) {
        use crate::{
            badge::BadgeSource,
            pane::{DockVisibility, View},
        };
        let ws = self.stoat.active_workspace_mut();
        for (_, dock) in &mut ws.docks {
            if matches!(&dock.view, View::Claude(id) if *id == session_id) {
                dock.visibility = DockVisibility::Open {
                    width: dock.default_width,
                };
            }
        }
        ws.badges.remove_by_source(BadgeSource::Claude(session_id));
        self.capture("show_claude_session");
    }

    pub fn claude_badge_state(
        &self,
        session_id: crate::host::ClaudeSessionId,
    ) -> Option<crate::badge::BadgeState> {
        let ws = self.stoat.active_workspace();
        let source = crate::badge::BadgeSource::Claude(session_id);
        ws.badges
            .find_by_source(source)
            .and_then(|id| ws.badges.get(id))
            .map(|b| b.state)
    }

    pub fn claude_badge_detail(&self, session_id: crate::host::ClaudeSessionId) -> Option<String> {
        let ws = self.stoat.active_workspace();
        let source = crate::badge::BadgeSource::Claude(session_id);
        ws.badges
            .find_by_source(source)
            .and_then(|id| ws.badges.get(id))
            .and_then(|b| b.detail.clone())
    }

    /// Sub-harness for driving Claude Code sessions in tests. Returned by
    /// short-lived reborrow; each method call on the harness re-acquires
    /// `&mut self`, mirroring the `HashMap::entry` pattern.
    pub fn claude(&mut self) -> claude::ClaudeHarness<'_> {
        claude::ClaudeHarness::new(self)
    }

    pub(crate) fn capture(&mut self, action: &str) {
        // First render spawns any pending parse jobs. Settling the test
        // scheduler runs them to completion. The second render polls the
        // results and installs them so the snapshot reflects them.
        let _ = self.stoat.render();
        self.scheduler.run_until_parked();
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

fn format_plain(frame: &Frame) -> String {
    let header = format_header(frame);
    format!("{header}\n---\n{}", frame.content)
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
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => "?".to_string(),
    };
    parts.push(key_name);
    parts.join("-")
}

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
        h.type_keys("z");
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
    fn snapshot_initial_plain() {
        let mut h = Stoat::test();
        h.assert_snapshot("initial_plain");
    }

    #[test]
    fn snapshot_initial_styled() {
        let mut h = Stoat::test();
        h.assert_snapshot_styled("initial_styled");
    }

    #[test]
    fn snapshot_space_mode() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.assert_snapshot("space_mode");
    }

    #[test]
    fn type_action_direct() {
        let mut h = Stoat::test();
        h.type_action("SetMode(space)");
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.mode, "space");
    }

    #[test]
    fn type_action_quit_from_space() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.type_action("Quit");
    }

    #[test]
    #[should_panic(expected = "unreachable")]
    fn type_action_unreachable_panics() {
        let mut h = Stoat::test();
        h.type_action("NonExistentAction");
    }

    #[test]
    fn snapshot_split_right() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.assert_snapshot("split_right");
    }

    #[test]
    fn snapshot_split_down() {
        let mut h = Stoat::test();
        h.type_action("SplitDown()");
        h.assert_snapshot("split_down");
    }

    #[test]
    fn snapshot_nested_splits() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("SplitDown()");
        h.assert_snapshot("nested_splits");
    }

    #[test]
    fn snapshot_three_columns() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("SplitRight()");
        h.assert_snapshot("three_columns");
    }

    #[test]
    fn snapshot_close_returns_to_single() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("ClosePane()");
        h.assert_snapshot("close_returns_to_single");
    }

    #[test]
    fn snapshot_split_right_focus_left() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("FocusLeft()");
        h.assert_snapshot("split_right_focus_left");
    }

    /// Seed a file in the harness' fake filesystem and return its path.
    /// Replaces the old tempdir-plus-real-fs helper; all tests that went
    /// through the old helper now exercise the same IO boundary that
    /// production code uses in tests ([`crate::host::FakeFs`]).
    fn write_file(h: &TestHarness, name: &str, content: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from("/test").join(name);
        h.fake_fs.insert_file(&path, content.as_bytes());
        path
    }

    #[test]
    fn open_file_shows_in_focused_pane() {
        let mut h = Stoat::test();
        let path = write_file(&h, "test.txt", "hello world");

        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_shows_in_focused_pane");
    }

    #[test]
    fn open_file_replaces_focused_pane_does_not_split() {
        let mut h = Stoat::test();
        let a = write_file(&h, "a.txt", "file A");
        let b = write_file(&h, "b.txt", "file B");

        h.open_file(&a);
        h.open_file(&b);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_replaces_focused_pane");
    }

    #[test]
    fn split_then_open_creates_multi_pane_layout() {
        let mut h = Stoat::test();
        let a = write_file(&h, "a.txt", "AAA");
        let b = write_file(&h, "b.txt", "BBB");
        let c = write_file(&h, "c.txt", "CCC");

        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.type_action("SplitRight()");
        h.open_file(&c);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 3);
        h.assert_snapshot("split_then_open_three");
    }

    #[test]
    fn open_missing_file_creates_empty_buffer() {
        let path = std::path::PathBuf::from("/test/does_not_exist.txt");

        let mut h = Stoat::test();
        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
    }

    #[test]
    fn command_palette_opens_file_end_to_end() {
        let mut h = Stoat::test();
        let path = write_file(&h, "palette_target.txt", "loaded via palette");
        let path_str = path.to_str().expect("utf8 path");

        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text(path_str);
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        assert!(
            frame.content.contains("loaded via palette"),
            "buffer not visible in frame:\n{}",
            frame.content
        );
    }

    #[test]
    fn command_palette_escape_cancels() {
        let mut h = Stoat::test();
        h.type_text(":Open");
        h.type_keys("escape");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn snapshot_command_palette_filter_empty() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.assert_snapshot("command_palette_filter_empty");
    }

    #[test]
    fn snapshot_command_palette_filter_typing() {
        let mut h = Stoat::test();
        h.type_text(":Foc");
        h.assert_snapshot("command_palette_filter_typing");
    }

    #[test]
    fn snapshot_command_palette_filter_narrows_to_one() {
        let mut h = Stoat::test();
        h.type_text(":quit");
        h.assert_snapshot("command_palette_filter_narrows_to_one");
    }

    #[test]
    fn snapshot_command_palette_collect_args_empty() {
        let mut h = Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.assert_snapshot("command_palette_collect_args_empty");
    }

    #[test]
    fn snapshot_command_palette_collect_args_typing() {
        let mut h = Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text("/tmp/example.rs");
        h.assert_snapshot("command_palette_collect_args_typing");
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
    fn snapshot_open_rust_file_highlights() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.rs", "fn main() {\n    let x = \"hi\";\n}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_highlights_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.rs", "fn main() {\n    let x = \"hi\";\n}\n");

        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_rust_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_json_file_highlights() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.json", "{\n  \"a\": 1\n}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_json_file_highlights");
    }

    #[test]
    fn snapshot_open_json_file_highlights_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.json", "{\n  \"a\": 1\n}\n");

        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_json_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_markdown_file_highlights() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.md", "# Title\n\nbody\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_highlights");
    }

    #[test]
    fn snapshot_open_markdown_file_highlights_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.md", "# Title\n\nbody\n");

        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_markdown_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_markdown_file_with_bold_inline() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "bold.md", "# Title\n\n**bold** text\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_with_bold_inline");
    }

    #[test]
    fn snapshot_open_markdown_file_with_bold_inline_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "bold.md", "# Title\n\n**bold** text\n");

        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_markdown_file_with_bold_inline_styled");
    }

    #[test]
    fn snapshot_open_unknown_extension_no_highlights() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "sample.txt", "fn main() {}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_unknown_extension_no_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_nested_captures() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "nested.rs", "fn main() { \"a\\nb\"; }\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_nested_captures");
    }

    #[test]
    fn snapshot_open_rust_file_then_edit_highlights() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "edit.rs", "fn a() {}\n");

        h.open_file(&path);
        // Insert a `let x = 1;` statement inside the body. Byte 8 is the
        // position right after the opening brace.
        h.edit_focused(8..8, " let x = 1; ");
        h.assert_snapshot("snapshot_open_rust_file_then_edit_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_then_edit_highlights_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "edit.rs", "fn a() {}\n");

        h.open_file(&path);
        h.edit_focused(8..8, " let x = 1; ");
        h.assert_snapshot_styled("snapshot_open_rust_file_then_edit_highlights_styled");
    }

    #[test]
    fn snapshot_open_rust_file_with_fold() {
        use stoat_text::Point;
        let mut h = TestHarness::with_size(40, 8);
        let path = write_file(
            &h,
            "folded.rs",
            "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n",
        );

        h.open_file(&path);
        // Fold the body of `fn b`: from after the open brace to just before
        // the close brace.
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot("snapshot_open_rust_file_with_fold");
    }

    #[test]
    fn snapshot_open_rust_file_with_fold_styled() {
        use stoat_text::Point;
        let mut h = TestHarness::with_size(40, 8);
        let path = write_file(
            &h,
            "folded.rs",
            "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n",
        );

        h.open_file(&path);
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot_styled("snapshot_open_rust_file_with_fold_styled");
    }

    #[test]
    fn snapshot_open_rust_file_nested_captures_styled() {
        let mut h = TestHarness::with_size(40, 6);
        let path = write_file(&h, "nested.rs", "fn main() { \"a\\nb\"; }\n");

        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_rust_file_nested_captures_styled");
    }

    #[test]
    fn snapshot_review_addition() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn b() {}\n",
            "fn a() {}\nfn new() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot_styled("review_addition");
    }

    #[test]
    fn snapshot_review_deletion() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn old() {}\nfn b() {}\n",
            "fn a() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot_styled("review_deletion");
    }

    #[test]
    fn snapshot_review_modification() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn main() {\n    let x = 1;\n}\n",
            "fn main() {\n    let x = 2;\n}\n",
        )]);
        h.assert_snapshot_styled("review_modification");
    }

    #[test]
    fn snapshot_review_multi_file() {
        let mut h = TestHarness::with_size(80, 16);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
            ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
        ]);
        h.assert_snapshot_styled("review_multi_file");
    }

    /// Straight move: two swapped top-level functions. The LCS pass
    /// pairs one as Unchanged; the other emerges through the move
    /// pass and every atom inside it renders with the move theme
    /// (cyan) rather than added/deleted colors. Paired layout + styled
    /// snapshot per `CLAUDE.md` so regressions in either positioning
    /// or color are caught.
    #[test]
    fn snapshot_review_move_straight() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}

fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}
";
        let rhs = "\
fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}

fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}
";
        h.open_review_from_texts(&[("swap.rs", base, rhs)]);
        h.assert_snapshot("review_move_straight");
        h.assert_snapshot_styled("review_move_straight_styled");
    }

    /// Cross-indentation move: a statement that lived at top level
    /// (inside `fn outer`) relocates into a different function's body
    /// (`fn wrapper`). The structural diff's `ContentId` ignores
    /// whitespace and parent context, so the moved statement is
    /// detected even though its indentation and containing scope both
    /// changed.
    #[test]
    fn snapshot_review_move_cross_indent() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn outer() {
    let relocated = compute(arg1, arg2, arg3);
}

fn wrapper() {
    println!(\"hello\");
}
";
        let rhs = "\
fn outer() {}

fn wrapper() {
    println!(\"hello\");
    let relocated = compute(arg1, arg2, arg3);
}
";
        h.open_review_from_texts(&[("nest.rs", base, rhs)]);
        h.assert_snapshot("review_move_cross_indent");
        h.assert_snapshot_styled("review_move_cross_indent_styled");
    }

    /// Ambiguous (N:1) consolidation: the same block appears in two
    /// LHS functions and gets factored into one shared RHS function.
    /// Both LHS copies render with the move theme because their
    /// `ContentId` matched the single RHS target; downstream, the
    /// move metadata's `sources` list records both candidate source
    /// locations so `JumpToNextMoveSource` / `JumpToPrevMoveSource`
    /// can cycle between them.
    #[test]
    fn snapshot_review_move_consolidation() {
        let mut h = TestHarness::with_size(100, 24);
        let base = "\
fn first() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn second() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}
";
        let rhs = "\
fn shared() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn first() {
    shared();
}

fn second() {
    shared();
}
";
        h.open_review_from_texts(&[("consolidate.rs", base, rhs)]);
        h.assert_snapshot("review_move_consolidation");
        h.assert_snapshot_styled("review_move_consolidation_styled");
    }

    #[test]
    fn snapshot_add_selection_below() {
        let mut h = TestHarness::with_size(20, 5);
        let path = write_file(&h, "sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot("add_selection_below");
    }

    #[test]
    fn snapshot_add_selection_below_styled() {
        let mut h = TestHarness::with_size(20, 5);
        let path = write_file(&h, "sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot_styled("add_selection_below_styled");
    }

    #[test]
    fn snapshot_shift_c_adds_selection_below_styled() {
        let mut h = TestHarness::with_size(20, 5);
        let path = write_file(&h, "sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("shift-C");
        h.assert_snapshot_styled("shift_c_adds_selection_below_styled");
    }

    #[test]
    fn snapshot_move_right() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot("snapshot_move_right");
    }

    #[test]
    fn snapshot_move_right_styled() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot_styled("snapshot_move_right_styled");
    }

    #[test]
    fn snapshot_move_down() {
        let mut h = TestHarness::with_size(20, 6);
        let path = write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot("snapshot_move_down");
    }

    #[test]
    fn snapshot_move_down_styled() {
        let mut h = TestHarness::with_size(20, 6);
        let path = write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot_styled("snapshot_move_down_styled");
    }

    #[test]
    fn snapshot_word_forward() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot("snapshot_word_forward");
    }

    #[test]
    fn snapshot_word_forward_styled() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot_styled("snapshot_word_forward_styled");
    }

    #[test]
    fn snapshot_word_end() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot("snapshot_word_end");
    }

    #[test]
    fn snapshot_word_end_styled() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot_styled("snapshot_word_end_styled");
    }

    #[test]
    fn snapshot_word_backward() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot("snapshot_word_backward");
    }

    #[test]
    fn snapshot_word_backward_styled() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot_styled("snapshot_word_backward_styled");
    }

    #[test]
    fn snapshot_word_forward_repeated() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot("snapshot_word_forward_repeated");
    }

    #[test]
    fn snapshot_word_forward_repeated_styled() {
        let mut h = TestHarness::with_size(30, 5);
        let path = write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot_styled("snapshot_word_forward_repeated_styled");
    }

    #[test]
    fn snapshot_multi_cursor_move_right() {
        let mut h = TestHarness::with_size(20, 6);
        let path = write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C l l");
        h.assert_snapshot("snapshot_multi_cursor_move_right");
    }

    #[test]
    fn snapshot_multi_cursor_move_right_styled() {
        let mut h = TestHarness::with_size(20, 6);
        let path = write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C l l");
        h.assert_snapshot_styled("snapshot_multi_cursor_move_right_styled");
    }

    #[test]
    fn snapshot_run_empty() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        h.assert_snapshot("run_empty");
    }

    #[test]
    fn snapshot_run_typed_input() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        h.type_text("echo hello");
        h.assert_snapshot("run_typed_input");
    }

    #[test]
    fn snapshot_run_typed_input_styled() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        h.type_text("echo hello");
        h.assert_snapshot_styled("run_typed_input_styled");
    }

    #[test]
    fn snapshot_run_output() {
        let mut h = TestHarness::with_size(60, 12);
        let id = h.open_run();
        h.submit_run("echo hello");
        h.inject_run_output(id, b"hello\n");
        h.inject_run_done(id, 0);
        h.assert_snapshot("run_output");
    }

    #[test]
    fn snapshot_run_output_styled() {
        let mut h = TestHarness::with_size(60, 12);
        let id = h.open_run();
        h.submit_run("echo hello");
        h.inject_run_output(id, b"hello\n");
        h.inject_run_done(id, 0);
        h.assert_snapshot_styled("run_output_styled");
    }

    #[test]
    fn snapshot_run_colored_output() {
        let mut h = TestHarness::with_size(60, 12);
        let id = h.open_run();
        h.submit_run("ls --color");
        h.inject_run_output(id, b"\x1b[32mgreen\x1b[0m \x1b[31mred\x1b[0m\n");
        h.inject_run_done(id, 0);
        h.assert_snapshot_styled("run_colored_output");
    }

    #[test]
    fn snapshot_run_exit_code() {
        let mut h = TestHarness::with_size(60, 12);
        let id = h.open_run();
        h.submit_run("false");
        h.inject_run_done(id, 1);
        h.assert_snapshot("run_exit_code");
    }

    #[test]
    fn snapshot_run_alt_screen_error() {
        let mut h = TestHarness::with_size(60, 12);
        let id = h.open_run();
        h.submit_run("vim");
        h.inject_run_output(id, b"\x1b[?1049h");
        h.assert_snapshot("run_alt_screen_error");
    }

    #[test]
    fn snapshot_run_multiple_blocks() {
        let mut h = TestHarness::with_size(60, 16);
        let id = h.open_run();
        h.submit_run("echo one");
        h.inject_run_output(id, b"one\n");
        h.inject_run_done(id, 0);
        h.submit_run("echo two");
        h.inject_run_output(id, b"two\n");
        h.inject_run_done(id, 0);
        h.assert_snapshot("run_multiple_blocks");
    }

    // --- Claude Code badge tests ---

    use crate::{
        badge::BadgeState,
        host::{AgentMessage, ClaudeSessionId, FakeClaudeCode},
        test_harness::claude::ResultSpec,
    };

    /// Badge tests need a non-visible Claude session: opened, moved into
    /// the right dock, then toggled through Minimized to Hidden. The badge
    /// machinery predates pane-based Claude and the layout expectations
    /// track the dock overlay.
    fn setup_hidden_claude_session(h: &mut TestHarness) -> ClaudeSessionId {
        let id = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        id
    }

    #[test]
    fn badge_appears_when_not_visible() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        assert!(h.claude_badge_state(id).is_none());

        h.claude().get_session(id).thinking("let me think");

        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));
    }

    #[test]
    fn badge_detail_updates_with_tool() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("hmm");
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));

        h.claude()
            .get_session(id)
            .read("/tmp/example.txt")
            .pending();
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Read".into()));

        h.claude().get_session(id).text("done reading");
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_completes_on_result() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 1000,
            num_turns: 1,
        });
        assert_eq!(
            h.claude_badge_state(id),
            Some(crate::badge::BadgeState::Complete)
        );
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_errors_on_error() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude()
            .get_session(id)
            .thinking("work")
            .error("rate limit");

        assert_eq!(
            h.claude_badge_state(id),
            Some(crate::badge::BadgeState::Error)
        );
        assert_eq!(h.claude_badge_detail(id), Some("rate limit".into()));
    }

    #[test]
    fn badge_removed_when_session_shown() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_some());

        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn no_badge_when_visible() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.show_claude_session(id);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn badge_reappears_after_hide() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.show_claude_session(id);
        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_none());

        // Hide the dock (Open -> Minimized -> Hidden)
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);

        h.claude()
            .get_session(id)
            .edit("/tmp/file.txt", "old", "new")
            .pending();
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Edit".into()));
    }

    #[test]
    fn init_and_unknown_inert() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).init();
        assert!(h.claude_badge_state(id).is_none());

        h.claude()
            .get_session(id)
            .raw(AgentMessage::Unknown { raw: "{}".into() });
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn multiple_sessions_independent() {
        let mut h = TestHarness::default();
        let id_a = setup_hidden_claude_session(&mut h);
        let id_b = h.create_background_session(FakeClaudeCode::new());

        h.claude().get_session(id_a).thinking("a");
        h.claude().get_session(id_b).bash("echo hi").pending();

        assert_eq!(h.claude_badge_state(id_a), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_a), Some("thinking".into()));
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_b), Some("Bash".into()));

        // Complete session A, B stays active.
        h.claude().get_session(id_a).result();
        assert_eq!(
            h.claude_badge_state(id_a),
            Some(crate::badge::BadgeState::Complete)
        );
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
    }

    #[test]
    fn snapshot_badge_active_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let id = setup_hidden_claude_session(&mut h);
        h.claude().get_session(id).thinking("work");
        h.assert_snapshot_styled("badge_active_styled");
    }

    #[test]
    fn snapshot_badge_complete_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let id = setup_hidden_claude_session(&mut h);
        h.claude()
            .get_session(id)
            .thinking("work")
            .result_with(ResultSpec {
                cost_usd: 0.01,
                duration_ms: 1000,
                num_turns: 1,
            });
        h.assert_snapshot_styled("badge_complete_styled");
    }

    #[test]
    fn snapshot_dock_open_overlay() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        h.assert_snapshot("dock_open_overlay");
    }

    #[test]
    fn snapshot_dock_open_overlay_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        h.assert_snapshot_styled("dock_open_overlay_styled");
    }

    #[test]
    fn snapshot_dock_minimized_overlay() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        h.assert_snapshot("dock_minimized_overlay");
    }

    #[test]
    fn snapshot_dock_minimized_overlay_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        h.assert_snapshot_styled("dock_minimized_overlay_styled");
    }

    #[test]
    fn snapshot_dock_overlays_split_panes() {
        let mut h = TestHarness::new_with_settings(
            80,
            10,
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(stoat_config::ClaudePlacement::DockRight),
            },
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let _ = h.claude().open();
        h.assert_snapshot("dock_overlays_split_panes");
    }

    #[test]
    fn result_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });
        assert_eq!(
            h.claude_badge_state(id),
            Some(crate::badge::BadgeState::Complete)
        );
    }

    #[test]
    fn error_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).error("failed");
        assert_eq!(
            h.claude_badge_state(id),
            Some(crate::badge::BadgeState::Error)
        );
        assert_eq!(h.claude_badge_detail(id), Some("failed".into()));
    }

    #[test]
    fn visible_session_result_removes_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_some());

        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn claude_panel_pairs_tool_use_and_result() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .bash("ls -la")
            .result("file1\nfile2\nfile3");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("Bash(ls -la)"),
            "expected tool header: {}",
            frame.content
        );
        assert!(
            frame.content.contains("file1 (+2 more lines)"),
            "expected tool result preview: {}",
            frame.content
        );
    }

    #[test]
    fn claude_panel_collapses_thinking() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .thinking("line one\nline two\nline three");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("Thinking... (3 lines)"),
            "expected collapsed thinking: {}",
            frame.content
        );
    }

    #[test]
    fn claude_panel_clears_throbber_on_result() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.stoat
            .active_workspace_mut()
            .chats
            .get_mut(&id)
            .unwrap()
            .active_since = Some(std::time::Instant::now());

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });

        let chat = &h.stoat.active_workspace().chats[&id];
        assert!(
            chat.active_since.is_none(),
            "throbber state should clear on Result"
        );
    }

    fn line_index_containing(lines: &[&str], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| {
                panic!(
                    "needle {needle:?} not found in frame:\n{}",
                    lines.join("\n")
                )
            })
    }

    #[test]
    fn claude_panel_preserves_paragraph_breaks() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("Para one here.\n\nPara two here.");

        let frame = h.frames().last().expect("frame");
        let lines: Vec<&str> = frame.content.split('\n').collect();
        let idx_one = line_index_containing(&lines, "Para one here.");
        let idx_two = line_index_containing(&lines, "Para two here.");
        assert!(
            idx_two >= idx_one + 2,
            "expected blank separator row between paragraphs, got adjacent rows: {:?}",
            &lines[idx_one..=idx_two],
        );
    }

    #[test]
    fn claude_panel_preserves_leading_indent() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("normal line\n    indented line");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("    indented line"),
            "expected leading indent preserved: {}",
            frame.content,
        );
    }

    #[test]
    fn claude_panel_separates_tool_from_text() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("hello before tool")
            .bash("ls")
            .pending();

        let frame = h.frames().last().expect("frame");
        let lines: Vec<&str> = frame.content.split('\n').collect();
        let idx_text = line_index_containing(&lines, "hello before tool");
        let idx_tool = line_index_containing(&lines, "Bash(ls)");
        assert!(
            idx_tool >= idx_text + 2,
            "expected blank separator row between assistant text and tool call: {:?}",
            &lines[idx_text..=idx_tool],
        );
    }

    #[test]
    fn claude_panel_tool_use_prefix_distinct_from_user() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude().get_session(id).bash("ls").pending();

        let frame = h.frames().last().expect("frame");
        assert!(
            !frame.content.contains("> Bash("),
            "tool-use header must not share the `> ` user prefix: {}",
            frame.content,
        );
    }

    // --- Step-by-step chat replay snapshots ---

    #[test]
    fn chat_replay_real_session_ls_repo() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let final_text = "Here are the top-level files and directories in the \
                          repo:\n\n**Files:** `CLAUDE.md`, `CLAUDE.local.md`, \
                          `Cargo.lock`, `Cargo.toml`, `LICENSE`, `README.md`, \
                          `TODO.md`, `clippy.toml`, `config.stcfg`, `flake.lock`, \
                          `flake.nix`, `log.txt`, `rust-toolchain.toml`, \
                          `rustfmt.toml`, `stoat.log`, `test.csv`\n\n\
                          **Directories:** `action`, `agent`, `bin`, `config`, \
                          `examples`, `language`, `log`, `logs`, `references`, \
                          `scheduler`, `script`, `stoat`, `target`, \
                          `test_workspace`, `text`, `tmp`, `vendor`, `viewport`";
        let ls_output = "CLAUDE.local.md\nCLAUDE.md\nCargo.lock\nCargo.toml\n\
                         LICENSE\nREADME.md\nTODO.md\naction\nagent\nbin\n\
                         clippy.toml\nconfig\nconfig.stcfg\nexamples\n\
                         flake.lock\nflake.nix\nlanguage\nlog\nlog.txt\nlogs\n\
                         references\nrust-toolchain.toml\nrustfmt.toml\n\
                         scheduler\nscript\nstoat\nstoat.log\ntarget\ntest.csv\n\
                         test_workspace\ntext\ntmp\nvendor\nviewport";

        h.claude()
            .get_session(id)
            .text("\n\nWorking.")
            .snap_styled("chat_replay_real_session_ls_repo_step_01_turn1_text_working_styled")
            .result_with(ResultSpec {
                cost_usd: 0.0746,
                duration_ms: 1753,
                num_turns: 1,
            })
            .snap_styled("chat_replay_real_session_ls_repo_step_02_turn1_result_styled")
            .thinking("The user wants to see the files in the repo. Let me list them.")
            .snap_styled("chat_replay_real_session_ls_repo_step_03_turn2_thinking_styled")
            .bash("ls /Users/lee/projects/stoat")
            .snap_styled("chat_replay_real_session_ls_repo_step_04_turn2_tool_use_ls_styled")
            .result(ls_output)
            .snap_styled("chat_replay_real_session_ls_repo_step_05_turn2_tool_result_styled")
            .text(final_text)
            .snap_styled("chat_replay_real_session_ls_repo_step_06_turn2_final_text_styled")
            .result_with(ResultSpec {
                cost_usd: 0.1066,
                duration_ms: 7458,
                num_turns: 2,
            })
            .snap_styled("chat_replay_real_session_ls_repo_step_07_turn2_result_styled");

        h.assert_snapshot("chat_replay_real_session_ls_repo_final");
    }

    #[test]
    fn chat_replay_multiline_paragraphs() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("First paragraph.\n\nSecond paragraph.\n\nThird paragraph.")
            .snap_styled("chat_replay_multiline_paragraphs_step_01_text_styled");

        h.assert_snapshot("chat_replay_multiline_paragraphs_final");
    }

    #[test]
    fn chat_replay_long_line_wraps() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let long = "one two three four five six seven eight nine ten \
                    eleven twelve thirteen fourteen fifteen sixteen \
                    seventeen eighteen nineteen twenty twenty-one";

        h.claude()
            .get_session(id)
            .text(long)
            .snap_styled("chat_replay_long_line_wraps_step_01_long_text_styled");

        h.assert_snapshot("chat_replay_long_line_wraps_final");
    }

    #[test]
    fn chat_replay_indented_lines() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let text = "here is some code:\n    fn foo() {\n        bar();\n    }\nend.";

        h.claude()
            .get_session(id)
            .text(text)
            .snap_styled("chat_replay_indented_lines_step_01_indented_text_styled");

        h.assert_snapshot("chat_replay_indented_lines_final");
    }

    #[test]
    fn chat_replay_thinking_then_text() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .thinking("line one\nline two\nline three")
            .snap_styled("chat_replay_thinking_then_text_step_01_thinking_styled")
            .text("Done thinking.")
            .snap_styled("chat_replay_thinking_then_text_step_02_text_styled");

        h.assert_snapshot("chat_replay_thinking_then_text_final");
    }

    #[test]
    fn chat_replay_tool_use_no_result_yet() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .bash("ls")
            .snap_styled("chat_replay_tool_use_no_result_yet_step_01_tool_use_pending_styled")
            .pending();

        h.assert_snapshot("chat_replay_tool_use_no_result_yet_final");
    }

    #[test]
    fn chat_replay_partial_then_final_text() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .partial("Hello ")
            .snap_styled("chat_replay_partial_then_final_text_step_01_partial_chunk_1_styled")
            .partial("Hello world ")
            .snap_styled("chat_replay_partial_then_final_text_step_02_partial_chunk_2_styled")
            .partial("Hello world from Claude.")
            .snap_styled("chat_replay_partial_then_final_text_step_03_partial_chunk_3_styled")
            .text("Hello world from Claude.")
            .snap_styled("chat_replay_partial_then_final_text_step_04_final_text_styled");

        h.assert_snapshot("chat_replay_partial_then_final_text_final");
    }

    #[test]
    fn chat_replay_narrow_pane_wrap() {
        let mut h = TestHarness::with_size(40, 16);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("Working on it. This reply should wrap several times in a 40-col pane.")
            .snap_styled("chat_replay_narrow_pane_wrap_step_01_text_styled");

        h.assert_snapshot("chat_replay_narrow_pane_wrap_final");
    }

    /// End-to-end sequence the production `ClaudeCode` adapter emits for
    /// a streamed assistant response: cumulative `PartialText` blocks
    /// that grow with each delta, then the authoritative `Text` block on
    /// `message_stop`, then `Result` on turn completion. Each
    /// `PartialText` contains the full block-so-far so the UI can
    /// overwrite its live view on every event without having to
    /// concatenate raw chunks itself.
    #[test]
    fn chat_replay_streamed_text_then_final_and_result() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let part1 = "| Directory | Primary Language |\n|---|---|";
        let part2 = format!("{part1}\n| `action/` | Rust |\n| `agent/` | Rust |");
        let full_text = format!("{part2}\n| `vendor/` | (vendor deps) |\n| `viewport/` | Rust |");

        h.claude()
            .get_session(id)
            .partial(part1)
            .snap_styled(
                "chat_replay_streamed_text_then_final_and_result_step_01_partial_chunk_1_styled",
            )
            .partial(&part2)
            .snap_styled(
                "chat_replay_streamed_text_then_final_and_result_step_02_partial_chunk_2_styled",
            )
            .partial(&full_text)
            .snap_styled(
                "chat_replay_streamed_text_then_final_and_result_step_03_partial_chunk_3_styled",
            )
            .text(&full_text)
            .snap_styled(
                "chat_replay_streamed_text_then_final_and_result_step_04_final_text_styled",
            )
            .result_with(ResultSpec {
                cost_usd: 0.2133,
                duration_ms: 58335,
                num_turns: 3,
            })
            .snap_styled("chat_replay_streamed_text_then_final_and_result_step_05_result_styled");

        h.assert_snapshot("chat_replay_streamed_text_then_final_and_result_final");
    }

    // --- Claude placement tests ---

    use crate::{
        app::Stoat,
        pane::{DockSide, DockVisibility, View},
    };
    use stoat_config::ClaudePlacement;

    fn claude_panes(stoat: &Stoat) -> Vec<ClaudeSessionId> {
        stoat
            .active_workspace()
            .panes
            .split_panes()
            .filter_map(|(_, p)| match &p.view {
                View::Claude(id) => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn claude_docks(stoat: &Stoat) -> Vec<(DockSide, DockVisibility, u16)> {
        stoat
            .active_workspace()
            .docks
            .iter()
            .filter_map(|(_, d)| match &d.view {
                View::Claude(_) => Some((d.side, d.visibility, d.default_width)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn open_claude_defaults_to_pane() {
        let mut h = TestHarness::default();
        let id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![id]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn open_claude_honors_dock_right_setting() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let _id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Right, DockVisibility::Open { width: 40 }, 40)]
        );
    }

    #[test]
    fn open_claude_honors_dock_left_setting() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockLeft),
        });
        let _id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Left, DockVisibility::Open { width: 40 }, 40)]
        );
    }

    #[test]
    fn open_claude_twice_focuses_existing_pane() {
        let mut h = TestHarness::default();
        let first = h.claude().open();
        let second = h.claude().open();
        assert_eq!(first, second, "second open should reuse first session");
        assert_eq!(claude_panes(&h.stoat), vec![first]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_pane_moves_from_dock() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let id = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToPane);
        assert_eq!(claude_panes(&h.stoat), vec![id]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_dock_right_moves_from_pane() {
        let mut h = TestHarness::default();
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Right, DockVisibility::Open { width: 40 }, 40)]
        );
        let has_editor = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(_)));
        assert!(
            has_editor,
            "Claude was the only pane; moving to dock should leave a scratch editor in that slot"
        );
    }

    #[test]
    fn claude_to_dock_flips_between_sides() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let _id = h.claude().open();
        // Shrink dock so we can see the width is preserved across flips.
        for (_, dock) in &mut h.stoat.active_workspace_mut().docks {
            dock.visibility = DockVisibility::Open { width: 25 };
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockLeft);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Left, DockVisibility::Open { width: 25 }, 40)]
        );
    }

    #[test]
    fn claude_to_pane_when_no_session_is_noop() {
        let mut h = TestHarness::default();
        let effect = crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToPane);
        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_dock_right_keeps_other_panes_intact() {
        let mut h = TestHarness::default();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let _ = h.claude().open();
        // Now: left pane editor, right pane Claude (focused). Moving Claude
        // to the right dock should close the right pane, leaving just the
        // original editor pane.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        let editor_pane_count = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .filter(|(_, p)| matches!(p.view, View::Editor(_)))
            .count();
        assert_eq!(editor_pane_count, 1, "Claude's pane should have closed");
    }

    #[test]
    fn snapshot_claude_as_pane_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        h.assert_snapshot_styled("claude_as_pane_styled");
    }

    // --- ClaudeHarness session-tracking and transport coverage ---

    #[test]
    fn claude_harness_single_session_reports_id() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        assert_eq!(h.claude().session_ids(), vec![id]);
    }

    #[test]
    fn claude_harness_init_sessions_multiple_background() {
        let mut h = TestHarness::with_size(80, 24);
        let ids = h
            .claude()
            .init_sessions(["first session", "second session", "third session"]);
        assert_eq!(ids.len(), 3);
        let mut known = h.claude().session_ids();
        known.sort_by_key(|id| ids.iter().position(|i| i == id).unwrap_or(usize::MAX));
        assert_eq!(known, ids);
    }

    #[test]
    fn claude_harness_list_sessions_reflects_seeded_summaries() {
        use crate::host::ClaudeCodeHost;

        let mut h = TestHarness::with_size(80, 24);
        h.claude()
            .init_sessions(["alpha".to_string(), "beta".to_string()]);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let summaries = rt
            .block_on(h.fake_claude_host.list_sessions())
            .expect("list_sessions");

        let titles: Vec<&str> = summaries.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["alpha", "beta"]);
        let expected_ids: Vec<&str> = summaries.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(expected_ids, vec!["sess-01", "sess-02"]);
    }

    #[test]
    fn claude_harness_say_flows_through_real_polling() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        h.claude().get_session(id).say("Hello from the fake.");

        let ws = h.stoat.active_workspace();
        let chat = ws.chats.get(&id).expect("chat state");
        let has_text = chat.messages.iter().any(|m| {
            matches!(
                &m.content,
                crate::claude_chat::ChatMessageContent::Text(t)
                    if t == "Hello from the fake."
            )
        });
        assert!(
            has_text,
            "expected text message to land in chat state via the polling path"
        );
        let has_turn_complete = chat.messages.iter().any(|m| {
            matches!(
                &m.content,
                crate::claude_chat::ChatMessageContent::TurnComplete { .. }
            )
        });
        assert!(has_turn_complete, "say() should emit a Result message");
    }

    #[test]
    fn claude_harness_bash_tool_pair_populates_kind_and_content() {
        use crate::host::{ToolCallContent, ToolKind};

        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        h.claude()
            .get_session(id)
            .bash("ls /tmp")
            .result("one\ntwo");

        let ws = h.stoat.active_workspace();
        let chat = ws.chats.get(&id).expect("chat state");
        let use_msg = chat
            .messages
            .iter()
            .find_map(|m| match &m.content {
                crate::claude_chat::ChatMessageContent::ToolUse { name, input, id } => {
                    Some((name.clone(), input.clone(), id.clone()))
                },
                _ => None,
            })
            .expect("ToolUse in chat");
        assert_eq!(use_msg.0, "Bash");
        assert!(use_msg.1.contains("ls /tmp"), "input JSON: {}", use_msg.1);
        assert!(
            use_msg.2.starts_with("toolu_"),
            "tool id {} should match toolu_* shape",
            use_msg.2
        );

        let result_msg = chat
            .messages
            .iter()
            .find_map(|m| match &m.content {
                crate::claude_chat::ChatMessageContent::ToolResult { id, content } => {
                    Some((id.clone(), content.clone()))
                },
                _ => None,
            })
            .expect("ToolResult in chat");
        assert_eq!(result_msg.0, use_msg.2, "result id pairs with use id");
        assert_eq!(result_msg.1, "one\ntwo");

        let _ = (
            ToolKind::Execute,
            ToolCallContent::Text {
                text: String::new(),
            },
        );
    }

    #[test]
    fn claude_harness_stream_message_emits_partials() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        h.claude()
            .get_session(id)
            .stream_message("The quick brown fox jumps over the lazy dog.");

        let ws = h.stoat.active_workspace();
        let chat = ws.chats.get(&id).expect("chat state");
        let has_final_text = chat.messages.iter().any(|m| {
            matches!(
                &m.content,
                crate::claude_chat::ChatMessageContent::Text(t)
                    if t == "The quick brown fox jumps over the lazy dog."
            )
        });
        assert!(has_final_text, "final Text should land in chat history");
        let has_turn_complete = chat.messages.iter().any(|m| {
            matches!(
                &m.content,
                crate::claude_chat::ChatMessageContent::TurnComplete { .. }
            )
        });
        assert!(
            has_turn_complete,
            "stream_message should terminate the turn with Result"
        );
        assert!(
            chat.streaming_text.is_none(),
            "streaming_text cleared after final Text: {:?}",
            chat.streaming_text
        );
    }

    #[test]
    fn claude_harness_tool_pending_leaves_result_absent() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").pending();

        let ws = h.stoat.active_workspace();
        let chat = ws.chats.get(&id).expect("chat state");
        let result_count = chat
            .messages
            .iter()
            .filter(|m| {
                matches!(
                    m.content,
                    crate::claude_chat::ChatMessageContent::ToolResult { .. }
                )
            })
            .count();
        assert_eq!(
            result_count, 0,
            "pending() should NOT emit a ToolResult message"
        );
    }

    /// Two hunks separated by unchanged context; cursor defaults to the
    /// first chunk. Verifies the cyan current-chunk gutter and the
    /// default progress footer.
    const REVIEW_TWO_HUNK_BASE: &str =
        "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
    const REVIEW_TWO_HUNK_BUFFER: &str =
        "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nT\n";

    #[test]
    fn snapshot_review_session_open() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.assert_snapshot("review_session_open");
        h.assert_snapshot_styled("review_session_open_styled");
    }

    #[test]
    fn snapshot_review_navigate_next() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("n");
        h.assert_snapshot("review_navigate_next");
        h.assert_snapshot_styled("review_navigate_next_styled");
    }

    #[test]
    fn snapshot_review_stage_current_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n");
        h.assert_snapshot("review_stage_current_chunk");
        h.assert_snapshot_styled("review_stage_current_chunk_styled");
    }

    #[test]
    fn snapshot_review_unstage_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("u n");
        h.assert_snapshot("review_unstage_chunk");
        h.assert_snapshot_styled("review_unstage_chunk_styled");
    }

    #[test]
    fn snapshot_review_toggle_cycles_binary() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("Space");
        {
            let ws = h.stoat.active_workspace();
            let session = ws.review.as_ref().expect("session");
            let id = session.cursor.current.expect("current chunk");
            assert_eq!(
                session.chunk(id).unwrap().status,
                crate::review_session::ChunkStatus::Staged,
            );
        }
        h.type_keys("Space");
        {
            let ws = h.stoat.active_workspace();
            let session = ws.review.as_ref().expect("session");
            let id = session.cursor.current.expect("current chunk");
            assert_eq!(
                session.chunk(id).unwrap().status,
                crate::review_session::ChunkStatus::Unstaged,
            );
        }
    }

    #[test]
    fn snapshot_review_skip_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("shift-S n");
        h.assert_snapshot("review_skip_chunk");
        h.assert_snapshot_styled("review_skip_chunk_styled");
    }

    #[test]
    fn snapshot_review_progress_footer() {
        let mut h = TestHarness::with_size(120, 30);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n");
        h.assert_snapshot("review_progress_footer");
        h.assert_snapshot_styled("review_progress_footer_styled");
    }

    #[test]
    fn snapshot_review_complete_state() {
        let mut h = TestHarness::with_size(120, 30);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n s");
        h.assert_snapshot("review_complete_state");
        h.assert_snapshot_styled("review_complete_state_styled");
        {
            let ws = h.stoat.active_workspace();
            let session = ws.review.as_ref().expect("session");
            assert!(session.is_complete());
            let has_badge = ws
                .badges
                .find_by_source(crate::badge::BadgeSource::Review)
                .is_some();
            assert!(has_badge, "complete review should surface a badge");
        }
    }

    #[test]
    fn review_close_restores_normal_mode() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        assert_eq!(h.stoat.mode, "review");
        h.type_keys("q");
        assert_eq!(h.stoat.mode, "normal");
        assert!(h.stoat.active_workspace().review.is_none());
    }

    #[test]
    fn snapshot_review_multi_file_navigation() {
        let mut h = TestHarness::with_size(80, 20);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
            ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
        ]);
        h.type_keys("n");
        {
            let ws = h.stoat.active_workspace();
            let session = ws.review.as_ref().expect("session");
            let chunk = session.current().expect("current");
            assert_eq!(chunk.file_index, 1);
            assert_eq!(chunk.chunk_index_in_file, 0);
        }
        h.assert_snapshot("review_multi_file_navigation");
        h.assert_snapshot_styled("review_multi_file_navigation_styled");
    }

    #[test]
    fn review_via_git_host_builds_session_from_working_tree() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session created by OpenReview");
        assert_eq!(session.files.len(), 1);
        assert_eq!(
            session.files[0].path,
            std::path::PathBuf::from("/work/a.rs")
        );
        assert_eq!(session.files[0].base_text.as_str(), REVIEW_TWO_HUNK_BASE);
        assert_eq!(
            session.files[0].buffer_text.as_str(),
            REVIEW_TWO_HUNK_BUFFER
        );
        assert_eq!(session.order.len(), 2);
        assert_eq!(h.stoat.mode, "review");
    }

    #[test]
    fn review_via_git_host_no_repo_is_noop() {
        let mut h = TestHarness::with_size(80, 14);
        // No repo registered; open_review should bail cleanly.
        h.stoat.open_review();
        assert!(h.stoat.active_workspace().review.is_none());
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn review_refresh_via_git_carries_status() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        let first_chunk_id = h.stoat.active_workspace().review.as_ref().unwrap().order[0];
        h.stoat
            .active_workspace_mut()
            .review
            .as_mut()
            .unwrap()
            .set_status(first_chunk_id, ChunkStatus::Staged);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRefresh);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session still present");
        assert_eq!(session.order.len(), 2);
        let statuses: Vec<_> = session
            .order
            .iter()
            .map(|id| session.chunks.get(id).unwrap().status)
            .collect();
        assert_eq!(
            statuses,
            vec![ChunkStatus::Staged, ChunkStatus::Pending],
            "first chunk's Staged decision should survive refresh; second should default to Pending",
        );
    }

    #[test]
    fn review_via_git_host_multi_file() {
        let mut h = TestHarness::with_size(80, 20);
        h.stage_review_scenario(
            "/work",
            &[
                ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
                ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
            ],
        );
        h.stoat.open_review();
        h.settle();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session");
        assert_eq!(session.files.len(), 2);
        assert_eq!(session.files[0].rel_path, "a.rs");
        assert_eq!(session.files[1].rel_path, "b.rs");
        assert!(session.order.len() >= 2);
    }

    #[test]
    fn stage_scenario_with_staged_seeds_both_buckets() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario_with_staged(
            "/work",
            &[("a.rs", "v1\n", "v2\n")],
            &[("b.rs", "staged\n")],
        );
        let repo =
            crate::host::GitHost::discover(&*h.fake_git, std::path::Path::new("/work")).unwrap();
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 2);
        let mut abs_paths: Vec<_> = changed.iter().map(|f| f.path.clone()).collect();
        abs_paths.sort();
        assert_eq!(abs_paths[0], std::path::PathBuf::from("/work/a.rs"));
        assert_eq!(abs_paths[1], std::path::PathBuf::from("/work/b.rs"));
    }

    #[test]
    fn open_agent_edit_review_via_helper_builds_session() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "", "added\n")]);
        let session = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .expect("session via helper");
        assert_eq!(session.files.len(), 2);
    }

    #[test]
    fn open_commit_review_via_helper_builds_session() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);
        h.open_commit_review("/work", "c2");
        let session = h.stoat.active_workspace().review.as_ref().unwrap();
        assert_eq!(session.files[0].buffer_text.as_str(), "v2\n");
    }

    #[test]
    fn review_mode_capital_a_triggers_apply() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        h.type_keys("A");

        let patches = h.fake_git.applied_patches(std::path::Path::new("/work"));
        assert_eq!(
            patches.len(),
            1,
            "expected one staged patch, got {patches:?}"
        );
    }

    #[test]
    fn review_mode_lowercase_r_triggers_refresh() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        let before_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;

        h.type_keys("r");

        let after_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;
        assert_ne!(
            before_editor, after_editor,
            "refresh must rebuild session + editor"
        );

        let session = h.stoat.active_workspace().review.as_ref().unwrap();
        assert_eq!(
            session.chunks[&session.order[0]].status,
            ChunkStatus::Staged,
            "refresh must carry staged status"
        );
    }

    #[test]
    fn scan_commit_builds_session_from_commit_vs_parent() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: std::path::PathBuf::from("/work"),
            sha: "c2".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for commit");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.files[0].base_text.as_str(), "v1\n");
        assert_eq!(session.files[0].buffer_text.as_str(), "v2\n");
        match &session.source {
            crate::review_session::ReviewSource::Commit { sha, .. } => {
                assert_eq!(sha, "c2")
            },
            other => panic!("unexpected source: {other:?}"),
        }
    }

    #[test]
    fn scan_commit_root_diffs_against_empty_tree() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("root", &[("a.rs", "initial\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: std::path::PathBuf::from("/work"),
            sha: "root".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for root commit");
        assert_eq!(session.files[0].base_text.as_str(), "");
        assert_eq!(session.files[0].buffer_text.as_str(), "initial\n");
    }

    #[test]
    fn scan_commit_range_spans_multiple_commits() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n"), ("b.rs", "new\n")])
            .commit_with_parent(
                "c3",
                "c2",
                &[("a.rs", "v3\n"), ("b.rs", "new\n"), ("c.rs", "added\n")],
            );

        let action = stoat_action::OpenReviewCommitRange {
            workdir: std::path::PathBuf::from("/work"),
            from: "c1".into(),
            to: "c3".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for range");
        let rels: Vec<_> = session.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(rels.contains(&"a.rs"), "a.rs must be in range: {rels:?}");
        assert!(rels.contains(&"b.rs"), "b.rs must be in range: {rels:?}");
        assert!(rels.contains(&"c.rs"), "c.rs must be in range: {rels:?}");
    }

    #[test]
    fn scan_agent_edits_builds_session_without_repo() {
        use std::sync::Arc;
        let mut h = TestHarness::with_size(80, 14);
        let action = stoat_action::OpenReviewAgentEdits {
            edits: vec![
                stoat_action::AgentEdit {
                    path: std::path::PathBuf::from("/proposed/a.rs"),
                    base_text: Arc::new("old text\n".to_string()),
                    proposed_text: Arc::new("new text\n".to_string()),
                },
                stoat_action::AgentEdit {
                    path: std::path::PathBuf::from("/proposed/b.rs"),
                    base_text: Arc::new("".to_string()),
                    proposed_text: Arc::new("added\n".to_string()),
                },
            ],
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for agent edits");
        assert_eq!(session.files.len(), 2);
        assert_eq!(session.files[0].base_text.as_str(), "old text\n");
        assert_eq!(session.files[0].buffer_text.as_str(), "new text\n");
        assert_eq!(session.files[1].base_text.as_str(), "");
        assert_eq!(session.files[1].buffer_text.as_str(), "added\n");
    }

    #[test]
    fn review_refresh_recomputes_commit_source() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\nline2\nline3\nline4\nline5\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "VX\nline2\nline3\nline4\nline5\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: std::path::PathBuf::from("/work"),
            sha: "c2".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        h.set_review_status(0, ChunkStatus::Staged);
        h.dispatch_review_refresh();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session survives refresh");
        assert_eq!(
            session.chunks[&session.order[0]].status,
            ChunkStatus::Staged
        );
    }

    #[test]
    fn review_apply_emits_patch_per_staged_chunk() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        h.set_review_status(1, ChunkStatus::Staged);
        h.dispatch_review_apply();

        let by_path = h
            .fake_git
            .applied_patches_by_path(std::path::Path::new("/work"));
        assert_eq!(
            by_path.len(),
            2,
            "two staged chunks must produce two patches: {by_path:#?}"
        );
        for (abs, patch) in &by_path {
            assert_eq!(abs, &std::path::PathBuf::from("/work/a.rs"));
            assert!(patch.contains("--- a/a.rs"), "unexpected patch: {patch}");
            assert!(patch.contains("+++ b/a.rs"), "unexpected patch: {patch}");
            assert!(patch.contains("@@ "), "missing hunk header: {patch}");
        }
    }

    #[test]
    fn review_apply_skips_pending_unstaged_skipped() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Unstaged);
        h.set_review_status(1, ChunkStatus::Skipped);
        h.dispatch_review_apply();

        assert!(
            h.fake_git
                .applied_patches(std::path::Path::new("/work"))
                .is_empty(),
            "non-staged chunks must not produce patches"
        );
    }

    #[test]
    fn review_apply_with_nothing_staged_is_noop() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.dispatch_review_apply();
        assert!(h
            .fake_git
            .applied_patches(std::path::Path::new("/work"))
            .is_empty());

        let ws = h.stoat.active_workspace();
        assert!(
            ws.badges
                .find_by_source(crate::badge::BadgeSource::Review)
                .is_none(),
            "nothing staged must not create a badge"
        );
    }

    #[test]
    fn review_apply_surfaces_failure_badge() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.fake_git
            .add_repo("/work")
            .fail_apply_with("simulated backend failure");
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        let chunk0_id = h.stoat.active_workspace().review.as_ref().unwrap().order[0];
        h.dispatch_review_apply();

        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("error badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, crate::badge::BadgeState::Error);
        assert_eq!(
            badge.detail.as_deref(),
            Some("simulated backend failure"),
            "detail must carry the backend message"
        );

        // Failed chunk remains Staged; user can retry.
        let session = ws.review.as_ref().unwrap();
        assert_eq!(
            session.chunks[&chunk0_id].status,
            ChunkStatus::Staged,
            "failed chunks must not be cleared"
        );
    }

    #[test]
    fn review_apply_auto_refreshes_on_full_success() {
        use crate::review_session::ChunkStatus;
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        h.set_review_status(1, ChunkStatus::Staged);

        let before_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;
        h.dispatch_review_apply();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session still present");
        assert_ne!(
            before_editor, session.view_editor,
            "auto-refresh must install a fresh editor via review_refresh"
        );

        let statuses: Vec<_> = session
            .order
            .iter()
            .map(|id| session.chunks[id].status)
            .collect();
        assert_eq!(
            statuses,
            vec![ChunkStatus::Staged, ChunkStatus::Staged],
            "carried statuses must survive auto-refresh"
        );

        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("complete badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, crate::badge::BadgeState::Complete);
        assert!(
            badge.label.contains("applied 2"),
            "badge must report count: {}",
            badge.label
        );
        assert_eq!(
            h.fake_git
                .applied_patches(std::path::Path::new("/work"))
                .len(),
            2,
            "both staged patches must have reached apply_to_index"
        );
    }

    #[test]
    fn open_file_via_fs_host_reads_from_fake_fs() {
        let mut h = Stoat::test();
        h.fake_fs
            .insert_file("/work/hello.txt", b"greetings from fake fs");
        h.stoat.open_file(std::path::Path::new("/work/hello.txt"));
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get(editor_id).unwrap();
        let buffer = ws.buffers.get(editor.buffer_id).unwrap();
        let guard = buffer.read().unwrap();
        assert_eq!(
            guard.snapshot.visible_text.to_string(),
            "greetings from fake fs"
        );
    }
}
