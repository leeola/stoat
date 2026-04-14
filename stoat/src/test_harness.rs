#![allow(dead_code)]

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
    collections::{BTreeMap, HashSet, VecDeque},
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
    stoat: Stoat,
    #[allow(dead_code)]
    scheduler: Arc<TestScheduler>,
    fake_claude_host: Arc<crate::host::FakeClaudeCodeHost>,
    frames: Vec<Frame>,
    last_buffer: Option<Buffer>,
    step: usize,
    sub_frame: usize,
}

impl TestHarness {
    fn new(width: u16, height: u16) -> Self {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let fake_claude_host = Arc::new(crate::host::FakeClaudeCodeHost::new());
        let mut stoat = Stoat::new(executor, Settings::default(), std::path::PathBuf::new());
        stoat.set_claude_code_host(fake_claude_host.clone());
        stoat.update(Event::Resize(width, height));

        let mut harness = Self {
            stoat,
            scheduler,
            fake_claude_host,
            frames: Vec::new(),
            last_buffer: None,
            step: 0,
            sub_frame: 0,
        };
        harness.capture("resize");
        harness
    }

    pub fn with_size(width: u16, height: u16) -> Self {
        Self::new(width, height)
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

    pub fn settle(&mut self) {
        self.scheduler.run_until_parked();
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
            display_map::{BlockPlacement, BlockProperties, BlockStyle, RenderBlock},
            editor_state::EditorState,
            pane::View,
            review::{self, ReviewRow},
        };
        use ratatui::{style::Style, text::Line};

        let mut review_rows: Vec<ReviewRow> = Vec::new();
        let mut blocks: Vec<BlockProperties> = Vec::new();
        let mut current_row: u32 = 0;

        for &(path, base, buffer) in files {
            let lang = self.stoat.language_registry.for_path(path.as_ref());
            let hunks = review::extract_review_hunks(lang.as_ref(), base, buffer, 3);
            if hunks.is_empty() {
                continue;
            }
            let lang_name = lang.as_ref().map(|l| l.name).unwrap_or("");
            let total = hunks.len();
            for (i, hunk) in hunks.iter().enumerate() {
                let label = format!("{path} --- {}/{total} --- {lang_name}", i + 1);
                let render: RenderBlock = {
                    let label = label.clone();
                    Arc::new(move |_ctx| {
                        vec![Line::styled(
                            label.clone(),
                            Style::default().fg(Color::Yellow),
                        )]
                    })
                };
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Above(current_row),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render,
                    diff_status: None,
                    priority: 0,
                });
                current_row += hunk.rows.len() as u32;
                review_rows.extend(hunk.rows.iter().cloned());
            }
        }

        let placeholder = " \n".repeat(review_rows.len().saturating_sub(1)) + " ";
        let executor = self.stoat.executor.clone();
        let ws = self.stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.new_scratch();
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..0, &placeholder);
            guard.dirty = false;
        }
        let mut editor = EditorState::new(buffer_id, buffer, executor);
        editor.display_map.insert_blocks(blocks);
        editor.review_rows = Some(review_rows);

        let new_id = ws.editors.insert(editor);
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Editor(new_id);
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
        self.fake_claude_host.push_session(fake);
        crate::action_handlers::dispatch(&mut self.stoat, &stoat_action::OpenClaude);
        self.stoat.complete_pending_claude_sessions();
        let id = self
            .stoat
            .active_workspace()
            .claude_chat
            .expect("OpenClaude should set claude_chat");
        self.capture("open_claude");
        id
    }

    pub fn create_background_session(
        &mut self,
        fake: crate::host::FakeClaudeCode,
    ) -> crate::host::ClaudeSessionId {
        let id = self.stoat.claude_sessions_mut().reserve_slot();
        let session: Arc<dyn crate::host::ClaudeCodeSession> = Arc::new(fake);
        self.stoat.claude_sessions_mut().fill_slot(id, session);
        id
    }

    pub fn inject_claude_message(
        &mut self,
        session_id: crate::host::ClaudeSessionId,
        message: &crate::host::AgentMessage,
    ) {
        self.stoat.handle_claude_message(session_id, message);
        self.capture("inject_claude_message");
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

    fn capture(&mut self, action: &str) {
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

    fn write_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write test file");
        path
    }

    #[test]
    fn open_file_shows_in_focused_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "test.txt", "hello world");

        let mut h = Stoat::test();
        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_shows_in_focused_pane");
    }

    #[test]
    fn open_file_replaces_focused_pane_does_not_split() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.txt", "file A");
        let b = write_file(dir.path(), "b.txt", "file B");

        let mut h = Stoat::test();
        h.open_file(&a);
        h.open_file(&b);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_replaces_focused_pane");
    }

    #[test]
    fn split_then_open_creates_multi_pane_layout() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.txt", "AAA");
        let b = write_file(dir.path(), "b.txt", "BBB");
        let c = write_file(dir.path(), "c.txt", "CCC");

        let mut h = Stoat::test();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.txt");

        let mut h = Stoat::test();
        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
    }

    #[test]
    fn command_palette_opens_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "palette_target.txt", "loaded via palette");
        let path_str = path.to_str().expect("utf8 path");

        let mut h = Stoat::test();
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
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "sample.rs",
            "fn main() {\n    let x = \"hi\";\n}\n",
        );

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_highlights_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "sample.rs",
            "fn main() {\n    let x = \"hi\";\n}\n",
        );

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_rust_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_json_file_highlights() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.json", "{\n  \"a\": 1\n}\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_json_file_highlights");
    }

    #[test]
    fn snapshot_open_json_file_highlights_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.json", "{\n  \"a\": 1\n}\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_json_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_markdown_file_highlights() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.md", "# Title\n\nbody\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_highlights");
    }

    #[test]
    fn snapshot_open_markdown_file_highlights_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.md", "# Title\n\nbody\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_markdown_file_highlights_styled");
    }

    #[test]
    fn snapshot_open_markdown_file_with_bold_inline() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "bold.md", "# Title\n\n**bold** text\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_with_bold_inline");
    }

    #[test]
    fn snapshot_open_markdown_file_with_bold_inline_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "bold.md", "# Title\n\n**bold** text\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot_styled("snapshot_open_markdown_file_with_bold_inline_styled");
    }

    #[test]
    fn snapshot_open_unknown_extension_no_highlights() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.txt", "fn main() {}\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_unknown_extension_no_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_nested_captures() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "nested.rs", "fn main() { \"a\\nb\"; }\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_nested_captures");
    }

    #[test]
    fn snapshot_open_rust_file_then_edit_highlights() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "edit.rs", "fn a() {}\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        // Insert a `let x = 1;` statement inside the body. Byte 8 is the
        // position right after the opening brace.
        h.edit_focused(8..8, " let x = 1; ");
        h.assert_snapshot("snapshot_open_rust_file_then_edit_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_then_edit_highlights_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "edit.rs", "fn a() {}\n");

        let mut h = TestHarness::with_size(40, 6);
        h.open_file(&path);
        h.edit_focused(8..8, " let x = 1; ");
        h.assert_snapshot_styled("snapshot_open_rust_file_then_edit_highlights_styled");
    }

    #[test]
    fn snapshot_open_rust_file_with_fold() {
        use stoat_text::Point;
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "folded.rs",
            "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n",
        );

        let mut h = TestHarness::with_size(40, 8);
        h.open_file(&path);
        // Fold the body of `fn b`: from after the open brace to just before
        // the close brace.
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot("snapshot_open_rust_file_with_fold");
    }

    #[test]
    fn snapshot_open_rust_file_with_fold_styled() {
        use stoat_text::Point;
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "folded.rs",
            "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n",
        );

        let mut h = TestHarness::with_size(40, 8);
        h.open_file(&path);
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot_styled("snapshot_open_rust_file_with_fold_styled");
    }

    #[test]
    fn snapshot_open_rust_file_nested_captures_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "nested.rs", "fn main() { \"a\\nb\"; }\n");

        let mut h = TestHarness::with_size(40, 6);
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

    #[test]
    fn snapshot_add_selection_below() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.txt", "abcd\nefgh\nijkl\n");

        let mut h = TestHarness::with_size(20, 5);
        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot("add_selection_below");
    }

    #[test]
    fn snapshot_add_selection_below_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.txt", "abcd\nefgh\nijkl\n");

        let mut h = TestHarness::with_size(20, 5);
        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot_styled("add_selection_below_styled");
    }

    #[test]
    fn snapshot_shift_c_adds_selection_below_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "sample.txt", "abcd\nefgh\nijkl\n");

        let mut h = TestHarness::with_size(20, 5);
        h.open_file(&path);
        h.type_keys("shift-C");
        h.assert_snapshot_styled("shift_c_adds_selection_below_styled");
    }

    #[test]
    fn snapshot_move_right() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "hello world\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot("snapshot_move_right");
    }

    #[test]
    fn snapshot_move_right_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "hello world\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot_styled("snapshot_move_right_styled");
    }

    #[test]
    fn snapshot_move_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "abc\ndef\nghi\n");
        let mut h = TestHarness::with_size(20, 6);
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot("snapshot_move_down");
    }

    #[test]
    fn snapshot_move_down_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "abc\ndef\nghi\n");
        let mut h = TestHarness::with_size(20, 6);
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot_styled("snapshot_move_down_styled");
    }

    #[test]
    fn snapshot_word_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot("snapshot_word_forward");
    }

    #[test]
    fn snapshot_word_forward_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot_styled("snapshot_word_forward_styled");
    }

    #[test]
    fn snapshot_word_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot("snapshot_word_end");
    }

    #[test]
    fn snapshot_word_end_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot_styled("snapshot_word_end_styled");
    }

    #[test]
    fn snapshot_word_backward() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot("snapshot_word_backward");
    }

    #[test]
    fn snapshot_word_backward_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot_styled("snapshot_word_backward_styled");
    }

    #[test]
    fn snapshot_word_forward_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot("snapshot_word_forward_repeated");
    }

    #[test]
    fn snapshot_word_forward_repeated_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "foo bar baz\n");
        let mut h = TestHarness::with_size(30, 5);
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot_styled("snapshot_word_forward_repeated_styled");
    }

    #[test]
    fn snapshot_multi_cursor_move_right() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "abc\ndef\nghi\n");
        let mut h = TestHarness::with_size(20, 6);
        h.open_file(&path);
        h.type_keys("C l l");
        h.assert_snapshot("snapshot_multi_cursor_move_right");
    }

    #[test]
    fn snapshot_multi_cursor_move_right_styled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "s.txt", "abc\ndef\nghi\n");
        let mut h = TestHarness::with_size(20, 6);
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
    };

    fn setup_claude_session(h: &mut TestHarness) -> ((), ClaudeSessionId) {
        let id = h.open_claude_with_fake(FakeClaudeCode::new());
        // Open -> Minimized -> Hidden: hide the dock so badge tests
        // start with a non-visible session.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        ((), id)
    }

    #[test]
    fn badge_appears_when_not_visible() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        assert!(h.claude_badge_state(id).is_none());

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "let me think".into(),
                signature: "sig".into(),
            },
        );

        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));
    }

    #[test]
    fn badge_detail_updates_with_tool() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "hmm".into(),
                signature: "sig".into(),
            },
        );
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));

        h.inject_claude_message(
            id,
            &AgentMessage::ToolUse {
                id: "toolu_1".into(),
                name: "Read".into(),
                input: "{}".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Read".into()));

        h.inject_claude_message(
            id,
            &AgentMessage::Text {
                text: "done reading".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_completes_on_result() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));

        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 1000,
                num_turns: 1,
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Complete));
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_errors_on_error() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );

        h.inject_claude_message(
            id,
            &AgentMessage::Error {
                message: "rate limit".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Error));
        assert_eq!(h.claude_badge_detail(id), Some("rate limit".into()));
    }

    #[test]
    fn badge_removed_when_session_shown() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        assert!(h.claude_badge_state(id).is_some());

        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn no_badge_when_visible() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.show_claude_session(id);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn badge_reappears_after_hide() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        // Show the dock
        h.show_claude_session(id);

        // Activity while visible -- no badge
        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        assert!(h.claude_badge_state(id).is_none());

        // Hide the dock (Open -> Minimized -> Hidden)
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);

        // New activity while hidden -- badge appears
        h.inject_claude_message(
            id,
            &AgentMessage::ToolUse {
                id: "toolu_1".into(),
                name: "Edit".into(),
                input: "{}".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Edit".into()));
    }

    #[test]
    fn init_and_unknown_inert() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Init {
                session_id: "test".into(),
                model: "test-model".into(),
                tools: vec![],
            },
        );
        assert!(h.claude_badge_state(id).is_none());

        h.inject_claude_message(id, &AgentMessage::Unknown { raw: "{}".into() });
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn multiple_sessions_independent() {
        let mut h = TestHarness::default();
        let (_, id_a) = setup_claude_session(&mut h);
        let id_b = h.create_background_session(FakeClaudeCode::new());

        h.inject_claude_message(
            id_a,
            &AgentMessage::Thinking {
                text: "a".into(),
                signature: "sig".into(),
            },
        );
        h.inject_claude_message(
            id_b,
            &AgentMessage::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: "{}".into(),
            },
        );

        assert_eq!(h.claude_badge_state(id_a), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_a), Some("thinking".into()));
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_b), Some("Bash".into()));

        // Complete session A, B stays active
        h.inject_claude_message(
            id_a,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 500,
                num_turns: 1,
            },
        );
        assert_eq!(h.claude_badge_state(id_a), Some(BadgeState::Complete));
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
    }

    #[test]
    fn snapshot_badge_active_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let (_, id) = setup_claude_session(&mut h);
        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        h.assert_snapshot_styled("badge_active_styled");
    }

    #[test]
    fn snapshot_badge_complete_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let (_, id) = setup_claude_session(&mut h);
        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 1000,
                num_turns: 1,
            },
        );
        h.assert_snapshot_styled("badge_complete_styled");
    }

    #[test]
    fn snapshot_dock_open_overlay() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.open_claude_with_fake(FakeClaudeCode::new());
        h.assert_snapshot("dock_open_overlay");
    }

    #[test]
    fn snapshot_dock_open_overlay_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.open_claude_with_fake(FakeClaudeCode::new());
        h.assert_snapshot_styled("dock_open_overlay_styled");
    }

    #[test]
    fn snapshot_dock_minimized_overlay() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.open_claude_with_fake(FakeClaudeCode::new());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        h.assert_snapshot("dock_minimized_overlay");
    }

    #[test]
    fn snapshot_dock_minimized_overlay_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.open_claude_with_fake(FakeClaudeCode::new());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        h.assert_snapshot_styled("dock_minimized_overlay_styled");
    }

    #[test]
    fn snapshot_dock_overlays_split_panes() {
        let mut h = TestHarness::with_size(80, 10);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let _ = h.open_claude_with_fake(FakeClaudeCode::new());
        h.assert_snapshot("dock_overlays_split_panes");
    }

    #[test]
    fn result_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 100,
                num_turns: 1,
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Complete));
    }

    #[test]
    fn error_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Error {
                message: "failed".into(),
            },
        );
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Error));
        assert_eq!(h.claude_badge_detail(id), Some("failed".into()));
    }

    #[test]
    fn visible_session_result_removes_badge() {
        let mut h = TestHarness::default();
        let (_, id) = setup_claude_session(&mut h);

        // Create badge while hidden
        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "work".into(),
                signature: "sig".into(),
            },
        );
        assert!(h.claude_badge_state(id).is_some());

        // Show session, badge removed
        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());

        // Result while visible - no badge created
        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 100,
                num_turns: 1,
            },
        );
        assert!(h.claude_badge_state(id).is_none());
    }

    fn setup_visible_claude_session(h: &mut TestHarness) -> ClaudeSessionId {
        h.open_claude_with_fake(FakeClaudeCode::new())
    }

    #[test]
    fn claude_panel_pairs_tool_use_and_result() {
        let mut h = TestHarness::with_size(80, 20);
        let id = setup_visible_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::ToolUse {
                id: "abc".into(),
                name: "Bash".into(),
                input: r#"{"command":"ls -la"}"#.into(),
            },
        );
        h.inject_claude_message(
            id,
            &AgentMessage::ToolResult {
                id: "abc".into(),
                content: "file1\nfile2\nfile3".into(),
            },
        );

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
        let id = setup_visible_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Thinking {
                text: "line one\nline two\nline three".into(),
                signature: "".into(),
            },
        );

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
        let id = setup_visible_claude_session(&mut h);

        h.stoat
            .active_workspace_mut()
            .chats
            .get_mut(&id)
            .unwrap()
            .active_since = Some(std::time::Instant::now());

        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.01,
                duration_ms: 100,
                num_turns: 1,
            },
        );

        let chat = &h.stoat.active_workspace().chats[&id];
        assert!(
            chat.active_since.is_none(),
            "throbber state should clear on Result"
        );
    }

    #[test]
    fn claude_panel_session_totals_in_header() {
        let mut h = TestHarness::with_size(80, 20);
        let id = setup_visible_claude_session(&mut h);

        h.inject_claude_message(
            id,
            &AgentMessage::Result {
                cost_usd: 0.0123,
                duration_ms: 1234,
                num_turns: 2,
            },
        );

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("$0.0123"),
            "expected cost in header: {}",
            frame.content
        );
        assert!(
            frame.content.contains("2 turns"),
            "expected turn count: {}",
            frame.content
        );
    }
}
