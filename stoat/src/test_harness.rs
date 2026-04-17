#![allow(dead_code)]

pub(crate) mod claude;

use crate::{
    app::{arg_as_str, Stoat, UpdateEffect},
    host::ClaudeSessionId,
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

    pub(crate) fn new_with_settings(width: u16, height: u16, settings: Settings) -> Self {
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

    /// Snapshot the rendered pane: characters plus inline ANSI SGR escapes
    /// for foreground/background color, modifiers (bold, reverse, etc.),
    /// and the cursor cell. Asserts positioning, style, and cursor state
    /// together.
    pub fn assert_snapshot(&mut self, name: &str) {
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

/// Seed a file in the harness' fake filesystem and return its path.
/// Replaces the old tempdir-plus-real-fs helper; all tests that went
/// through the old helper now exercise the same IO boundary that
/// production code uses in tests ([`crate::host::FakeFs`]).
pub(crate) fn write_file(h: &TestHarness, name: &str, content: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from("/test").join(name);
    h.fake_fs.insert_file(&path, content.as_bytes());
    path
}

/// Two hunks separated by unchanged context; cursor defaults to the
/// first chunk. Verifies the cyan current-chunk gutter and the
/// default progress footer.
pub(crate) const REVIEW_TWO_HUNK_BASE: &str =
    "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
pub(crate) const REVIEW_TWO_HUNK_BUFFER: &str =
    "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nT\n";

/// Badge tests need a non-visible Claude session: opened, moved into
/// the right dock, then toggled through Minimized to Hidden. The badge
/// machinery predates pane-based Claude and the layout expectations
/// track the dock overlay.
pub(crate) fn setup_hidden_claude_session(h: &mut TestHarness) -> ClaudeSessionId {
    let id = h.claude().open();
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
    id
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

    use crate::test_harness::claude::ResultSpec;

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
}
