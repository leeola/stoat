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
    frames: Vec<Frame>,
    last_buffer: Option<Buffer>,
    step: usize,
    sub_frame: usize,
}

impl TestHarness {
    fn new(width: u16, height: u16) -> Self {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let mut stoat = Stoat::new(executor);
        stoat.update(Event::Resize(width, height));

        let mut harness = Self {
            stoat,
            scheduler,
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
            match self.stoat.update(Event::Key(key)) {
                UpdateEffect::Redraw => self.maybe_capture(&desc),
                _ => {},
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
        insta::assert_snapshot!(name, text);
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
        insta::assert_snapshot!(name, text);
    }

    /// Edit the focused buffer at the given byte range, replacing it with
    /// `text`. Triggers a capture so the next render reflects the edit.
    /// Test-only helper for exercising the incremental reparse path.
    pub fn edit_focused(&mut self, range: std::ops::Range<usize>, text: &str) {
        let focused = self.stoat.panes.focus();
        let editor_id = match self.stoat.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("edit_focused: focused pane is not an editor"),
        };
        let editor = self
            .stoat
            .editors
            .get(editor_id)
            .expect("focused editor exists");
        let buffer = self
            .stoat
            .buffers
            .get(editor.buffer_id)
            .expect("buffer exists");
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
        let focused = self.stoat.panes.focus();
        let editor_id = match self.stoat.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("fold_focused: focused pane is not an editor"),
        };
        let editor = self
            .stoat
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
        let (buffer_id, buffer) = self.stoat.buffers.new_scratch();
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..0, &placeholder);
            guard.dirty = false;
        }
        let mut editor = EditorState::new(buffer_id, buffer, self.stoat.executor.clone());
        editor.display_map.insert_blocks(blocks);
        editor.review_rows = Some(review_rows);

        let new_id = self.stoat.editors.insert(editor);
        let focused = self.stoat.panes.focus();
        self.stoat.panes.pane_mut(focused).view = View::Editor(new_id);
        self.capture("open_review");
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
        let focused_id = self.stoat.panes.focus();
        let pane_count = self.stoat.panes.pane_count();
        let focused_pos = self
            .stoat
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
}
